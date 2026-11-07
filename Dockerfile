# syntax=docker/dockerfile:1.10
# Build: cargo-chef on rust:bookworm (glibc 2.36), run: distroless/cc (glibc, libgcc, no shell).
FROM rust:1.99-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p gitimage
COPY . .
RUN cargo build --release -p gitimage && cp target/release/gitimage /gitimage

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /gitimage /usr/local/bin/gitimage
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/gitimage"]
