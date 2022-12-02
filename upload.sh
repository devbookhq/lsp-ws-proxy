#!/bin/bash

set -euo pipefail

gsutil -h "Cache-Control:no-cache, max-age=0" \
  cp lsp-ws-proxy gs://devbook-environment-pipeline/lsp-ws-proxy
