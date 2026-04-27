#!/bin/sh
set -eu

cd "$(dirname "$0")/../.."
exec cargo xtask image ext4 --profile busybox "$@"
