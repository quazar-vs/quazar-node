# Документы на сайте (знакомство без private GitHub)

Гость читает **действующие** тексты с узла, не из закрытых репозиториев.

| Что | URL |
|-----|-----|
| Каталог | `GET /algonism/documents` |
| Тело документа | `GET /algonism/documents/:id` (`constitution`, `dna`, `ek`, …) |
| Совместимость | `GET /law/:filename` |
| Страница | https://quazar.website/documents.html |

Узел подтягивает эталон с GitHub (`GITHUB_TOKEN`) в кэш; сайт ходит только на узел.
