#!/bin/bash

PROVIDERS=(
    "prometheus"
    "elasticsearch"
    "loki"
    "sentry"
    "https"
    "cloudwatch"
    "parseable"
)

REPO_ROOT=`dirname "$0"`/..
PROVIDERS_DIR="${REPO_ROOT}/../providers"

CYAN='\033[0;36m'
WHITE='\033[0;37;1m'
NC='\033[0m'

set -e

if [ ! -d $PROVIDERS_DIR ]; then
    echo "Please make sure you have the providers repository checked out next"
    echo "to your fpd project."
    exit 1
fi

cd $PROVIDERS_DIR

printf "${CYAN}Compiling providers...${NC}\n"
cargo xtask build all

for provider in "${PROVIDERS[@]}"; do
    OUTPUT="../fpd/providers/${provider}.wasm"
    ARTIFACT="artifacts/${provider}.wasm"
    cp $ARTIFACT $OUTPUT
done

echo "Done."
