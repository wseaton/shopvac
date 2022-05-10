FROM rust:1.60.0 as builder
WORKDIR /usr/src/app
COPY src/  /usr/src/app
COPY Cargo.toml /usr/src/app
COPY Cargo.lock /usr/src/app

RUN cargo install --path .

FROM registry.access.redhat.com/ubi8/ubi-minimal:latest

COPY --from=builder /usr/local/cargo/bin/shopvac /usr/local/bin/shovac
COPY --from=builder  /usr/local/cargo/bin/shopvac-controller /usr/local/bin/shopvac-controller
ENTRYPOINT ["/usr/local/bin/shovac"]
