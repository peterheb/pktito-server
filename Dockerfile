# Build from the directory that holds both trees:
#   docker build -f pktito-server/Dockerfile -t pktito-server .
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends make
COPY libpktito /src/libpktito
COPY pktito-server /src/pktito-server
WORKDIR /src/pktito-server
RUN make -C ../libpktito libpktito.a && CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/pktito-server/target/release/pktito-server /usr/local/bin/
USER 65532:65532
EXPOSE 8000
ENTRYPOINT ["pktito-server", "-l", "0.0.0.0:8000"]
