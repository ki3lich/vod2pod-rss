# by using --platform=$BUILDPLATFORM we force the build step
# to always run on the native architecture of the build machine
# making the build time shorter
FROM --platform=$BUILDPLATFORM lukemathwalker/cargo-chef:latest-rust-1.88 AS chef
WORKDIR /app

# ----------
# Planner stage: generate the recipe.json for dependency caching
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ----------
# Builder stage: compile the application
FROM chef AS builder

ARG BUILDPLATFORM
ARG TARGETPLATFORM

# Find the right build target for rust
RUN if [ "$TARGETPLATFORM" = "linux/arm/v7" ]; then \
        export RUST_TARGET_PLATFORM=armv7-unknown-linux-gnueabihf; \
    elif [ "$TARGETPLATFORM" = "linux/arm64" ]; then \
        export RUST_TARGET_PLATFORM=aarch64-unknown-linux-gnu; \
    elif [ "$TARGETPLATFORM" = "linux/amd64" ]; then \
        export RUST_TARGET_PLATFORM=x86_64-unknown-linux-gnu; \
    else \
        export RUST_TARGET_PLATFORM=$(rustup target list --installed | head -n 1); \
    fi; \
    echo "choosen rust target: $RUST_TARGET_PLATFORM" ;\
    echo $RUST_TARGET_PLATFORM > /rust_platform.txt

RUN echo "I am running on $BUILDPLATFORM, building for $TARGETPLATFORM, rust target is $(cat /rust_platform.txt)"

RUN if echo $TARGETPLATFORM | grep -q 'arm'; then \
        echo 'Installing packages for ARM platforms...'; \
        apt-get update && apt-get install build-essential gcc gcc-arm* gcc-aarch* -y && apt-get clean; \
        echo 'gcc-arm* packages installed and cache cleaned.'; \
    fi

RUN rustup target add $(cat /rust_platform.txt)

# Copy the recipe.json and build dependencies (cached layer)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --target "$(cat /rust_platform.txt)" --recipe-path recipe.json

# Copy source code
COPY . .

# Inject version into the HTML template
ARG APP_VERSION
RUN if [ -n "$APP_VERSION" ]; then \
        sed -i "s|<!-- ###VERSION### -->|<small class=\"text-muted\"><br>v${APP_VERSION}</small>|" templates/index.html; \
    fi

RUN cargo build --release --target "$(cat /rust_platform.txt)"

RUN echo "final size of vod2pod:\n $(ls -lah /app/target/*/release/app)"

# ----------
# Runtime stage: this step will always run on the target architecture,
# so the build driver will need to be able to support runtime commands on it (es: using QEMU)
FROM --platform=$TARGETPLATFORM debian:bookworm-slim AS app

ARG BUILDPLATFORM
ARG TARGETPLATFORM

RUN echo "I am running on $BUILDPLATFORM, building for $TARGETPLATFORM"
COPY requirements.txt ./
# Install ffmpeg and yt-dlp
RUN apt-get update && \
    apt-get install -y --no-install-recommends unzip python3 curl ca-certificates ffmpeg && \
    export YT_DLP_VERSION=$(cat requirements.txt | grep yt-dlp | cut -d "=" -f3 | awk -F. '{printf "%d.%02d.%02d\n", $1, $2, $3}') && \
    curl -L https://github.com/yt-dlp/yt-dlp/releases/download/$YT_DLP_VERSION/yt-dlp -o /usr/local/bin/yt-dlp && \
    chmod a+rx /usr/local/bin/yt-dlp && \
    curl -fsSL https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip -o deno.zip && \
    unzip deno.zip -d /usr/local/bin && \
    rm deno.zip && \
    apt-get -y purge curl && \
    apt-get -y purge unzip && \
    apt-get -y autoremove && \
    apt-get -y clean && \
    rm -rf /var/lib/apt/lists/*

# Try to install deno with install script, do not fail if it does not work
RUN apt-get update && apt-get install -y unzip curl ca-certificates && \
    curl -fsSL https://deno.land/install.sh | DENO_INSTALL=/usr/local sh || true && \
    apt-get -y purge curl && \
    apt-get -y purge unzip && \
    apt-get -y autoremove && \
    apt-get -y clean && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/*/release/app /usr/local/bin/vod2pod
COPY --from=builder /app/templates/ ./templates

RUN if deno --version; then \
        echo "deno runs correctly"; \
    else \
        echo "deno not available"; \
    fi

RUN if vod2pod --version; then \
        echo "vod2pod starts correctly"; \
        exit 0; \
    else \
        echo "vod2pod did not start" 1>&2; \
        exit 1; \
    fi

EXPOSE 8080

CMD ["vod2pod"]
