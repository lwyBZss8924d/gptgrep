FROM python:3.12-slim
COPY --from=ghcr.io/astral-sh/uv:0.9.7 /uv /uvx /usr/local/bin/
WORKDIR /app
COPY . /tests/
ENV PYTHONDONTWRITEBYTECODE=1
RUN uv sync --project /tests --no-dev --python /usr/local/bin/python
RUN chmod +x /tests/test.sh
