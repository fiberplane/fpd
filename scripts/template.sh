#!/bin/bash
set -euo pipefail

#
# Replaces all environment variables from the input and writes the result to
# stdout.
#
#   Usage: template.sh <input>
#

main() {
    local input="${1?input not set}";

    >&2 echo "Templating input: \"$input\"";

    eval "cat <<EOF
$(cat "$input")
EOF
"
}

main "$*";
