# ---- 构建阶段：musl 静态编译 ----
FROM rust:1-alpine AS build
# ring（rustls 的加密库）需要 C 编译器
RUN apk add --no-cache build-base
WORKDIR /app

# 先只拷贝依赖清单并预编译依赖，源码改动时不用重新编译所有依赖
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# touch：确保 cargo 认为 main.rs 比占位版本新，从而重新编译
RUN touch src/main.rs && cargo build --release --locked

# ---- 运行阶段 ----
FROM alpine:3
# 连接微软的 wss 需要系统根证书
RUN apk add --no-cache ca-certificates \
    && adduser -D -H -u 10001 app
COPY --from=build /app/target/release/edge-tts-legado /usr/local/bin/edge-tts-legado

USER app

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -q -O /dev/null "http://127.0.0.1:${PORT}/health" || exit 1

CMD ["edge-tts-legado"]
