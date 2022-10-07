#!/bin/bash
set -euo pipefail

#
# Calls template.sh for all files. Outputs to stdout.
#
#   Usage: template_all.sh <input[]>
#

main() {
    local input="${1?input_dir not set}";

    DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"

    for input in $input; do
        "$DIR/template.sh" "$input";
    done
}

main "$*";
