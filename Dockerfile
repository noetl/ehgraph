FROM rust:1.99.1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p ehgraph-server

FROM debian:bookworm-slim
RUN useradd --system --create-home --home-dir /var/lib/ehgraph ehgraph \
    && mkdir -p /var/lib/ehgraph /etc/ehgraph \
    && chown -R ehgraph:ehgraph /var/lib/ehgraph /etc/ehgraph
COPY --from=build /src/target/release/ehgraph /usr/local/bin/ehgraph
USER ehgraph
EXPOSE 8080
VOLUME ["/var/lib/ehgraph"]
ENTRYPOINT ["ehgraph"]
CMD ["serve", "--config", "/etc/ehgraph/config.toml"]
