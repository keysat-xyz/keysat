# Licensing-service StartOS package build.
#
# Most of the build logic is shared across all StartOS packages and lives in
# `s9pk.mk`, which is copied from the `hello-world-startos` template. Pull
# that file in alongside this Makefile.
#
# Common targets:
#   make                  -- build for all supported architectures
#   make x86              -- build for x86_64 only
#   make arm              -- build for aarch64 only
#   make universal        -- build a single universal package
#   make install          -- install to the StartOS server referenced by
#                            your ~/.startos/developer.key.pem
#   make clean            -- wipe build artifacts
#
# Chain targets when needed: `make clean arm install`.

include s9pk.mk
