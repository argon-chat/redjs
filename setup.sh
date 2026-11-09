#!/usr/bin/env bash
set -euo pipefail
exec bun "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/scripts/setup.ts" "$@"
