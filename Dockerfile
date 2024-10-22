# we need the same debian version on the rust and varnish so
# that libssl-dev and libssl3 match
FROM rust:1.82-bookworm AS builder

ENV WORKDIR=/vmod_cloudbucket
WORKDIR ${WORKDIR}
ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
ENV CARGO_REGISTRY_DIR=${CARGO_HOME}/registry
ENV CARGO_TARGET_DIR=${WORKDIR}/target

RUN set -e; \
	curl -s https://packagecloud.io/install/repositories/varnishcache/varnish75/script.deb.sh | bash; \
	apt-get install -y varnish-dev clang libssl-dev;

COPY . .
RUN --mount=type=cache,sharing=locked,target=${CARGO_REGISTRY_DIR} \
    --mount=type=cache,sharing=locked,target=${CARGO_TARGET_DIR} \
	set -e; \
	cargo build --release;\
	mkdir -p /builder/vmods/; \
	cp ${CARGO_TARGET_DIR}/release/libvmod_cloudbucket.so /builder/vmods/;


# Run tests
#
# RUN --mount=type=cache,sharing=locked,target=${CARGO_REGISTRY_DIR} \
#     --mount=type=cache,sharing=locked,target=${CARGO_TARGET_DIR} \
#     export RUST_BACKTRACE=1 \
# 		cargo build \
# 		cargo test

FROM varnish:7.5
USER root
RUN set -e; \
	apt-get update; \
	apt-get install -y libssl3; \
	rm -rf /var/lib/apt/lists/*
COPY --from=builder /builder/vmods/libvmod_cloudbucket.so /usr/lib/varnish/vmods/
USER varnish
