#!/usr/bin/env bash
set -euo pipefail

archive="${TEST_SRCDIR}/${TEST_WORKSPACE}/tools/checkleft/starlark_text_fixture_package.tar.gz"

if [[ ! -f "${archive}" ]]; then
    echo "missing package archive: ${archive}" >&2
    exit 1
fi

entries="$(tar -tzf "${archive}" | sort)"

require_entry() {
    local expected="$1"
    if ! grep -Fxq "${expected}" <<<"${entries}"; then
        echo "missing archive entry: ${expected}" >&2
        echo "${entries}" >&2
        exit 1
    fi
}

require_entry "package.toml"
require_entry "lib/messages.checkleft"
require_entry "text/no_debug/check.checkleft"
require_entry "text/nested/no_todo/check.checkleft"

if grep -q "testdata" <<<"${entries}"; then
    echo "archive must not include author testdata" >&2
    echo "${entries}" >&2
    exit 1
fi
