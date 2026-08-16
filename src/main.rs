#![allow(dead_code)]
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use chrono::Utc;
use sqlx::PgPool;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

mod algonism;
mod auth;
mod auth_login;
mod agent_permissions;
mod applications;
mod block_producer;
mod blockchain;
mod candidacy;
mod candidates;
mod channel;
mod chain_events;
mod chat;
mod candidate_chat;
mod client_ip;
mod citizen;
mod citizenship_application;
mod citizenship_portal;
mod db;
mod email;
mod exchange;
mod crypto;
mod gossip;
mod handlers;
mod hermes_log;
mod http;
mod initiative;
mod keys;
mod mayor;
mod models;
mod node_generator;
mod nodes;
mod offices;
mod agents;
mod pending;
mod projection;
mod referendum;
mod register_verify;
mod response;
mod reward_svod;
mod vpn;
mod svod;
mod types;
mod validator;
mod votes;

#[cfg(test)]
mod integration_tests;

use auth::{
    assert_recaptcha_in_production, init_node_secret, init_reg_secret, node_secret, reg_secret,
    KeyStore,
};
use auth_login::{check_password_handler, login_handler, set_password_handler, sync_test_passwords};
use agent_permissions::{
    grant_permissions_handler, list_permissions_handler, revoke_permission_handler,
};
use citizenship_application::citizenship_application_handler;
use register_verify::{send_code_handler, verify_code_handler};
use vpn::{
    activate_vpn_handler, generate_vpn_key_handler, issue_ss_credential_handler,
    list_vpn_keys_handler, revoke_ss_credential_handler, revoke_vpn_key_handler, ss_status_handler,
    vpn_status_handler,
};
use crypto::assert_production_secrets;
use citizen::*;
use candidacy::{
    appoint_handler, get_candidacy_handler, list_candidacies_handler, nominate_handler, vote_handler,
};
use channel::{
    connect_handler, cross_channel_handler, disconnect_handler, events_handler, health_handler,
    post_message_handler, status_handler, ws_handler, ChannelHub,
};
use candidate_chat::{candidate_list_messages_handler, candidate_send_message_handler};
use chat::{
    admin_send_message_handler, background_chat_archive, gossip_chat_message_handler,
    list_history_handler, list_messages_handler, get_session_handler, search_db_handler,
    search_messages_handler, send_message_handler,
};
use initiative::{
    get_initiative_handler, list_initiatives_handler, propose_handler as propose_initiative_handler,
    vote_handler as vote_initiative_handler,
};
use referendum::{
    announce_handler, get_referendum_handler, list_referendums_handler,
    vote_handler as vote_referendum_handler,
};
use mayor::{
    approve_street_mayor_application_handler, create_election_handler, endorse_handler,
    finalize_handler, get_election_handler, list_elections_handler,
    list_street_mayor_applications_handler, mayor_application_status_handler, mayor_apply_handler,
    mayor_confirm_handler, mayor_node_started_handler, mayor_package_handler,
    mayor_setup_package_handler, nominate_handler as mayor_nominate_handler,
    reject_street_mayor_application_handler, start_election_handler, start_mayor_timeout_worker,
    street_mayor_application_handler, vote_handler as mayor_vote_handler,
};
use handlers::{background_sync, background_vpn_cleanup};
use http::{auth_middleware, build_cors_layer, public_rate_limit_middleware, rate_limit_middleware};
use nodes::{Node, NodeRegistry, NodeStatus};

pub use models::{Block, Event};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    node_registry: Arc<NodeRegistry>,
    pub node_id: String,
    pub channel_hub: Arc<ChannelHub>,
    /// Trusted proxies for XFF: `nodes` registry (non-local) + `QUAZAR_TRUSTED_PROXIES`.
    pub trusted_proxies: client_ip::TrustedProxyCache,
    /// In-RAM Algonism DNA (genetic memory); not stored on-chain.
    pub dna: algonism::DnaMemory,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    init_tracing();
    init_node_secret();
    init_reg_secret();
    assert_production_secrets(node_secret(), reg_secret());
    assert_recaptcha_in_production();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://quazar:quazar@localhost:5432/quazar".to_string());
    let node_id = std::env::var("QUAZAR_NODE_ID").unwrap_or_else(|_| "QZ-NODE".to_string());
    let node_url = std::env::var("QUAZAR_NODE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let port = std::env::var("QUAZAR_PORT").unwrap_or_else(|_| "8080".to_string());

    tracing::info!("Quazar Blockchain v0.7.0 starting");

    let pool = db::connect(&database_url)
        .await
        .expect("Failed to connect to PostgreSQL");
    db::run_migrations(&pool)
        .await
        .expect("Failed to run database migrations");
    if let Err(e) = agents::ensure_state_agents(&pool).await {
        tracing::warn!(error = %e, "failed to ensure state Algonism agents");
    }
    KeyStore::sync_from_env(&pool)
        .await
        .expect("Failed to sync API keys");
    sync_test_passwords(&pool)
        .await
        .expect("Failed to sync test citizen passwords");
    if let Err(e) = reward_svod::ensure_rewards_seeded(&pool).await {
        tracing::warn!(error = %e, "failed to seed svod reward defaults");
    } else {
        tracing::info!("Республиканский Свод Вознаграждений seeded / verified");
    }
    let pending_count = pending::count(&pool).await.unwrap_or(0);
    tracing::info!(pending_events = pending_count, "PostgreSQL connected");

    let node_registry = Arc::new(NodeRegistry::new(pool.clone()));

    let my_node = Node {
        id: node_id.clone(),
        url: node_url.clone(),
        public_key: None,
        status: NodeStatus::Alive,
        last_seen: Utc::now(),
        version: "0.7.0".to_string(),
    };
    let _ = node_registry.upsert_node(&my_node).await;

    if node_id != "QZ-NODE" {
        if let Err(e) = sqlx::query(
            "UPDATE nodes SET status = 'dead' WHERE id = 'QZ-NODE' AND url LIKE '%localhost%'",
        )
        .execute(&pool)
        .await
        {
            tracing::warn!(error = %e, "failed to retire legacy QZ-NODE registry entry");
        }
    }

    if let Ok(bootstrap) = std::env::var("QUAZAR_BOOTSTRAP_PEERS") {
        for entry in bootstrap.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((id, url)) = entry.split_once('@') else {
                tracing::warn!(entry = %entry, "invalid bootstrap peer format (use id@url)");
                continue;
            };
            if id == node_id {
                continue;
            }
            let peer = Node {
                id: id.to_string(),
                url: url.to_string(),
                public_key: None,
                status: NodeStatus::Alive,
                last_seen: Utc::now(),
                version: "0.7.0".to_string(),
            };
            match node_registry.upsert_node(&peer).await {
                Ok(()) => tracing::info!(peer_id = %id, url = %url, "bootstrap peer registered"),
                Err(e) => tracing::warn!(peer_id = %id, error = %e, "failed to register bootstrap peer"),
            }
        }
    }

    let trusted_proxies = client_ip::TrustedProxyCache::new(pool.clone());
    let dna = algonism::DnaMemory::new();
    if let Err(e) = algonism::load_dna_at_startup(&dna, &pool).await {
        tracing::warn!(error = %e, "Algonism DNA not loaded at startup (will retry on /algonism/identity|/awaken|/dna/reload)");
    }

    let state = Arc::new(AppState {
        db: pool,
        node_registry,
        node_id,
        channel_hub: Arc::new(ChannelHub::new()),
        trusted_proxies,
        dna,
    });

    // GitHub → node cache poll (DNA + laws). Never gossiped over P2P.
    algonism::spawn_reference_poller(state.clone());

    let state_clone = state.clone();
    tokio::spawn(async move {
        background_sync(state_clone).await;
    });

    let state_chat = state.clone();
    tokio::spawn(async move {
        background_chat_archive(state_chat).await;
    });

    start_mayor_timeout_worker(state.clone());

    let state_vpn = state.clone();
    tokio::spawn(async move {
        background_vpn_cleanup(state_vpn).await;
    });

    let state_lots = state.clone();
    tokio::spawn(async move {
        exchange::background_listing_expiry(state_lots).await;
    });

    let public_routes = Router::new()
        .route("/auth/login", post(login_handler))
        .route("/auth/check", get(check_password_handler))
        .route("/auth/register/send-code", post(send_code_handler))
        .route("/auth/register/verify-code", post(verify_code_handler))
        .route(
            "/api/citizenship-application",
            post(citizenship_application_handler),
        )
        .route("/api/application", post(citizenship_portal::submit_application))
        .route(
            "/api/application/verify",
            post(citizenship_portal::verify_application_code),
        )
        .route(
            "/api/my-applications",
            post(citizenship_portal::my_applications),
        )
        .route(
            "/api/application/:id",
            get(citizenship_portal::get_application_status),
        )
        .route("/api/public-config", get(applications::public_config_handler))
        .route("/api/public/stats", get(public_stats_handler))
        .route("/api/citizens", post(session_citizens_handler))
        .route("/health", get(health_handler))
        .route("/algonism/identity", get(algonism::identity_handler))
        .route("/algonism/awaken", get(algonism::awaken_handler))
        .route("/law/:filename", get(algonism::law_handler))
        .route("/citizen/apply", post(apply_for_citizenship))
        .route("/citizen/apply/citizenship", post(apply_for_citizenship))
        .route("/citizen/verify-email", post(verify_citizenship_email))
        .route("/api/channel/connect", post(connect_handler))
        .route("/api/channel/disconnect", post(disconnect_handler))
        .route("/api/channel/messages", post(post_message_handler))
        .route("/api/channel/cross-channel-messages", post(cross_channel_handler))
        .route("/api/channel/events", get(events_handler))
        .route("/api/channel/status", get(status_handler))
        .route("/api/channel/ws", get(ws_handler))
        .route("/api/search", get(search_db_handler))
        .route("/candidacy/list", get(list_candidacies_handler))
        .route("/candidacy/:id", get(get_candidacy_handler))
        .route("/initiative/list", get(list_initiatives_handler))
        .route("/initiative/:id", get(get_initiative_handler))
        .route("/referendum/list", get(list_referendums_handler))
        .route("/referendum/:id", get(get_referendum_handler))
        .route("/mayor/list", get(list_elections_handler))
        .route("/mayor/:id", get(get_election_handler))
        .route("/api/mayor/setup-package", get(mayor_setup_package_handler))
        .route(
            "/api/mayor/status/:id",
            get(mayor_application_status_handler),
        )
        .route(
            "/api/mayor/application",
            post(street_mayor_application_handler),
        )
        .route("/api/mayor/node-started", post(mayor_node_started_handler))
        .route("/api/mayor/confirm", post(mayor_confirm_handler))
        .route(
            "/api/chat/candidate/messages",
            post(candidate_list_messages_handler),
        )
        .route(
            "/api/chat/candidate/send",
            post(candidate_send_message_handler),
        )
        .route("/chat/admin/send", post(admin_send_message_handler))
        // Public abuse limits: register/apply 5/min, auth/application 10/min,
        // channel/search/admin-send 20/min (per IP).
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            public_rate_limit_middleware,
        ))
        .with_state(state.clone());

    let protected_routes = Router::new()
        .route("/algonism/dna/reload", post(algonism::reload_handler))
        .route("/blocks", get(handlers::get_blocks))
        .route("/events", get(handlers::get_events))
        .route("/events/gossip", post(handlers::gossip_event))
        .route("/chat/gossip", post(gossip_chat_message_handler))
        .route("/event", post(handlers::add_event))
        .route("/nodes", get(handlers::get_nodes))
        .route("/peers", post(handlers::add_peer))
        .route("/peers/network", post(handlers::add_peer_to_network))
        .route("/online", post(handlers::online_handler))
        .route("/offline", post(handlers::offline_handler))
        .route("/citizens/online", get(handlers::list_online_citizens))
        .route("/vote", post(handlers::cast_vote_handler))
        .route("/votes", post(votes::create_vote))
        .route("/votes", get(votes::list_votes))
        .route("/votes/finalize", post(votes::finalize_vote))
        .route("/exchange/requests", post(exchange::create_request))
        .route("/exchange/requests", get(exchange::list_requests))
        .route(
            "/exchange/requests/:id/sb-verify",
            post(exchange::sb_verify_request),
        )
        .route("/exchange/needs", post(exchange::create_need))
        .route("/exchange/needs", get(exchange::list_needs))
        .route("/exchange/listings", get(exchange::list_listings))
        .route("/exchange/listings/:id/buy", post(exchange::buy_listing))
        .route(
            "/exchange/listings/:id/extend",
            post(exchange::extend_listing),
        )
        .route("/exchange/sell-to-state", post(exchange::sell_to_state))
        .route("/exchange/reserve/dispose", post(exchange::dispose_reserve))
        .route("/exchange/transfers", post(exchange::create_lot_transfer))
        .route(
            "/exchange/transfers/:id/advance",
            post(exchange::advance_lot_transfer),
        )
        .route("/exchange/exports", post(exchange::create_lot_transfer))
        .route(
            "/exchange/exports/:id/advance",
            post(exchange::advance_lot_transfer),
        )
        .route("/exchange/node-lots", get(exchange::list_node_lots))
        .route("/exchange/node-lots", post(exchange::adjust_node_lot))
        .route("/exchange/deals/:id/cancel", post(exchange::cancel_deal))
        .route(
            "/exchange/security/tasks",
            get(exchange::list_security_tasks),
        )
        .route(
            "/exchange/security/tasks/:id/confirm",
            post(exchange::confirm_delivery),
        )
        .route("/exchange/inventory", get(exchange::get_inventory_handler))
        .route("/exchange/inventory", post(exchange::adjust_inventory))
        .route("/exchange/balance", get(exchange::get_balance_handler))
        .route("/exchange/balance/add", post(exchange::add_balance))
        .route("/offices/mayor", post(offices::appoint_mayor_handler))
        .route("/offices/mayor/:node_id", get(offices::get_mayor_office))
        .route(
            "/offices/mayor/:node_id/leave",
            post(offices::leave_mayor_handler),
        )
        .route("/city/appoint", post(offices::appoint_city_role))
        .route("/agents", get(agents::list_agents))
        .route("/agents/messages", post(agents::post_agent_message))
        .route("/agents/lawyer/impulse", post(agents::post_lawyer_impulse))
        .route(
            "/agents/city/ensure",
            post(agents::ensure_city_agents_handler),
        )
        // Temporary Passport compatibility (old P2P paths → listings/requests/deals)
        .route("/exchange/offers", get(exchange::legacy_get_offers))
        .route("/exchange/offer", post(exchange::legacy_create_offer))
        .route("/exchange/offer/:id", get(exchange::legacy_get_offer_by_id))
        .route("/exchange/offer/:id", delete(exchange::legacy_cancel_offer))
        .route("/exchange/order", post(exchange::legacy_create_order))
        .route("/exchange/orders", get(exchange::legacy_get_orders))
        .route("/api/channel/videos", post(reward_svod::create_video_handler))
        .route(
            "/api/channel/videos/:id/view",
            post(reward_svod::view_video_handler),
        )
        .route("/svod", get(svod::get_svod_handler))
        .route("/svod/rewards", get(svod::get_rewards_handler))
        .route("/svod/categories", get(svod::get_categories_handler))
        .route("/svod/service/:code", get(svod::get_service_handler))
        .route("/svod/admin/service", post(svod::create_service_handler))
        .route("/svod/admin/service/:code", put(svod::update_service_handler))
        .route("/svod/admin/service/:code", delete(svod::disable_service_handler))
        .route("/svod/admin/reward", post(reward_svod::update_reward_handler))
        .route("/auth/set-password", post(set_password_handler))
        .route("/keys", post(keys::create_api_key))
        .route("/keys", get(keys::list_api_keys))
        .route("/keys/revoke", post(keys::revoke_api_key))
        .route("/keys/sync", post(keys::sync_api_keys))
        .route("/keys/internal/export", get(keys::internal_export_keys))
        .route("/keys/internal/upsert", post(keys::internal_upsert_key))
        .route("/keys/internal/revoke", post(keys::internal_revoke_key))
        .route("/citizen/register", post(register_citizen))
        .route("/citizen/list", get(list_citizens))
        .route("/citizen/search", get(search_citizens))
        .route("/citizen/pending", get(list_pending_applications))
        .route("/citizen/apply/role", post(apply_for_role))
        .route("/api/applications", get(applications::list_applications))
        .route("/api/candidates", get(candidates::list_candidates_handler))
        .route(
            "/api/candidates/:id",
            get(candidates::get_candidate_handler),
        )
        .route(
            "/api/candidates/:id/approve",
            post(candidates::approve_candidate_handler),
        )
        .route(
            "/api/candidates/:id/reject",
            post(candidates::reject_candidate_handler),
        )
        .route(
            "/api/application/:id/approve",
            post(citizenship_portal::approve_application),
        )
        .route(
            "/api/application/:id/reject",
            post(citizenship_portal::reject_application),
        )
        .route("/citizen/key", post(generate_api_key))
        .route("/citizen/public-key", post(set_public_key))
        .route("/citizen/keys", get(list_api_keys))
        .route("/citizen/key/:id", delete(revoke_api_key))
        .route("/citizen/:id", get(get_citizen))
        .route("/citizen/:id/status", patch(update_status))
        .route("/citizen/:id/role", patch(update_role))
        .route("/citizen/:id/verify", post(verify_agent))
        .route("/citizen/:id/approve", post(approve_citizen))
        .route("/citizen/:id/passport", post(issue_passport))
        .route("/citizen/:id/passport/revoke", post(revoke_passport))
        .route("/candidacy/nominate", post(nominate_handler))
        // vote body: {"vote": "For" | "Against"}
        .route("/candidacy/:id/vote", post(vote_handler))
        .route("/candidacy/:id/appoint", post(appoint_handler))
        .route("/initiative/propose", post(propose_initiative_handler))
        // vote body: {"vote": "For" | "Against"}
        .route("/initiative/:id/vote", post(vote_initiative_handler))
        .route("/referendum/announce", post(announce_handler))
        // vote body: {"vote": "For" | "Against"}
        .route("/referendum/:id/vote", post(vote_referendum_handler))
        .route("/mayor/election", post(create_election_handler))
        .route("/mayor/nominate", post(mayor_nominate_handler))
        .route("/mayor/endorse", post(endorse_handler))
        .route("/mayor/:id/start", post(start_election_handler))
        .route("/mayor/:id/vote", post(mayor_vote_handler))
        .route("/mayor/:id/finalize", post(finalize_handler))
        .route("/api/mayor/apply", post(mayor_apply_handler))
        .route("/api/mayor/:id/package", get(mayor_package_handler))
        .route(
            "/api/mayor/applications",
            get(list_street_mayor_applications_handler),
        )
        .route(
            "/api/mayor/application/:id/approve",
            post(approve_street_mayor_application_handler),
        )
        .route(
            "/api/mayor/application/:id/reject",
            post(reject_street_mayor_application_handler),
        )
        .route("/chat/messages", get(list_messages_handler))
        .route("/chat/search", get(search_messages_handler))
        .route("/chat/send", post(send_message_handler))
        .route("/hermes/event-types", get(hermes_log::list_event_types_handler))
        .route("/hermes/event", post(hermes_log::log_event_handler))
        .route("/chat/session", get(get_session_handler))
        .route("/chat/history", get(list_history_handler))
        .route(
            "/admin/agent/:id/permissions",
            get(list_permissions_handler).post(grant_permissions_handler),
        )
        .route(
            "/admin/agent/:id/permissions/:perm",
            delete(revoke_permission_handler),
        )
        .route("/api/vpn/key", post(generate_vpn_key_handler))
        .route("/api/vpn/ss/credential", post(issue_ss_credential_handler))
        .route("/api/vpn/ss/revoke", post(revoke_ss_credential_handler))
        .route("/api/vpn/ss/status", get(ss_status_handler))
        .route("/api/vpn/status", get(vpn_status_handler))
        .route("/api/vpn/revoke", post(revoke_vpn_key_handler))
        .route("/api/vpn/keys", get(list_vpn_keys_handler))
        .route("/api/citizen/activate_vpn", post(activate_vpn_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .with_state(state.clone());

    let app = Router::new()
        .route("/status", get(handlers::status))
        .merge(public_routes)
        .merge(protected_routes)
        .layer(TraceLayer::new_for_http())
        .layer(build_cors_layer())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .with_state(state);

    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    tracing::info!(%addr, "listening");
    axum::serve(
        tokio::net::TcpListener::bind(&addr).await.unwrap(),
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
        .await
        .unwrap();
}
