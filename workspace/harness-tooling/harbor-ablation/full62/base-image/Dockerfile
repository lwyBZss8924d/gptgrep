# Both image references must include an immutable sha256 digest.
ARG UV_IMAGE
ARG PYTHON_IMAGE
FROM ${UV_IMAGE} AS uv
FROM ${PYTHON_IMAGE} AS common
ARG PYTHON_IMAGE
ARG UV_IMAGE
ARG DEBIAN_SNAPSHOT
ARG POPPLER_VERSION
ARG CODEX_SHA256
ARG BWRAP_SHA256
ARG TARGETPLATFORM
ENV PYTHONDONTWRITEBYTECODE=1 UV_PYTHON_DOWNLOADS=never
COPY --from=uv /uv /usr/local/bin/uv
COPY requirements.lock probe.py /usr/local/share/full62-base/
COPY codex /usr/local/bin/codex
COPY --chmod=0555 bwrap /usr/local/bin/codex-resources/bwrap
COPY helper-record.json COPYING.bubblewrap LICENSE.codex /usr/local/share/full62-sandbox-helper/
RUN test -n "$DEBIAN_SNAPSHOT" && test -n "$POPPLER_VERSION" && test -n "$CODEX_SHA256" \
    && . /etc/os-release && test "$VERSION_CODENAME" = bookworm \
    && printf '%s\n' \
      "deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT/ bookworm main" \
      "deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/$DEBIAN_SNAPSHOT/ bookworm-security main" \
      > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends "poppler-utils=$POPPLER_VERSION" \
    && rm -rf /var/lib/apt/lists/* \
    && uv pip sync --system --require-hashes --only-binary=:all: --no-cache \
      /usr/local/share/full62-base/requirements.lock \
    && printf '%s  %s\n' "$CODEX_SHA256" /usr/local/bin/codex | sha256sum --check - \
    && test -n "$BWRAP_SHA256" \
    && printf '%s  %s\n' "$BWRAP_SHA256" /usr/local/bin/codex-resources/bwrap | sha256sum --check - \
    && chmod 755 /usr/local/bin/codex \
    && test "$(codex --version)" = 'codex-cli 0.162.1' \
    && python3 /usr/local/share/full62-base/probe.py \
      --python-image "$PYTHON_IMAGE" --uv-image "$UV_IMAGE" \
      --debian-snapshot "$DEBIAN_SNAPSHOT" --poppler-version "$POPPLER_VERSION" \
      --platform "$TARGETPLATFORM" \
      > /usr/local/share/full62-base/provenance.json
WORKDIR /tmp

# Only this final target has the current product binary. No corpus/index is built in.
FROM common AS gptgrep
ARG GPTGREP_SHA256
COPY gptgrep /usr/local/bin/gptgrep
RUN test -n "$GPTGREP_SHA256" \
    && printf '%s  %s\n' "$GPTGREP_SHA256" /usr/local/bin/gptgrep | sha256sum --check - \
    && chmod 755 /usr/local/bin/gptgrep
