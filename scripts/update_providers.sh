#!/bin/bash

PROVIDERS=(
    "prometheus" 
    "elasticsearch" 
    "loki"
)

REPO_ROOT=`dirname "$0"`/..
PROVIDERS_REPO="${REPO_ROOT}/../providers"

CYAN='\033[0;36m'
WHITE='\033[0;37;1m'
NC='\033[0m'

which wasm-opt > /dev/null
if [ $? -eq 1 ]; then
    printf "Please make sure you have ${WHITE}wasm-opt${NC} installed and in your PATH.\n"
    echo "Make sure to install a recent version from: "
    echo "    https://github.com/WebAssembly/binaryen/releases"
    exit 1
fi

set -e

if [ ! -d $PROVIDERS_REPO ]; then
    echo "Please make sure you have the providers repository checked out next"
    echo "to your studio project."
    exit 1
fi

cd $PROVIDERS_REPO

git switch main
git pull

printf "${CYAN}Compiling providers...${NC}"
for provider in "${PROVIDERS[@]}"; do
    pushd "providers/$provider"
        cargo build --release
    popd
done

pwd

printf "${CYAN}Optimizing providers...${NC}\n"
for provider in "${PROVIDERS[@]}"; do
    # Optimize for performance
    wasm-opt -O2 -o "../proxy/providers/${provider}.wasm" "target/wasm32-unknown-unknown/release/${provider}_provider.wasm"
done

echo "Done."
