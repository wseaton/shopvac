FROM rust:1.60.0 as builder
WORKDIR /usr/src/app
COPY ./  /usr/src/app

RUN cargo install --path .

FROM fedora:latest

COPY --from=builder /usr/local/cargo/bin/shopvac /usr/local/bin/shopvac
COPY --from=builder  /usr/local/cargo/bin/shopvac-controller /usr/local/bin/shopvac-controller
ENTRYPOINT ["/usr/local/bin/shopvac"]