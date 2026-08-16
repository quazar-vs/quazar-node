FROM python:3.12-slim

WORKDIR /app
COPY server.py /app/server.py

ENV EXTERNAL_AGENTS_HOST=0.0.0.0
ENV EXTERNAL_AGENTS_PORT=8090
EXPOSE 8090

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8090/health', timeout=2)"

CMD ["python", "-u", "/app/server.py"]
