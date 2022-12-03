#!/bin/bash

set -euo pipefail

gsutil -h "Cache-Control:no-cache, max-age=0" \
  cp target/x86_64-unknown-linux-musl/release/lsp-ws-proxy gs://devbook-environment-pipeline
