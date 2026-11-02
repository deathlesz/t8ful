#!/bin/sh
# A minimal MCP stdio server used by the t8ful-mcp tests.
#
# Tests launch this through `bash` from PATH rather than relying on this line:
# a Nix build sandbox has no `/usr/bin/env`. The shebang is kept so the script
# still runs when invoked directly on a normal host.
#
# It speaks newline-delimited JSON-RPC on stdin/stdout, which is the framing
# `rmcp`'s `AsyncRwTransport` expects. Behaviour is selected by the first
# argument so one script covers the success path, error results, malformed
# schemas, oversized results, a server that dies mid-call, a server whose
# handshake is gated on a file, and a server that records the environment it was
# started with.
#
# Every mode answers `initialize` and `tools/list`; `tools/call` behaves per
# mode. `slow` holds the `initialize` answer until the path given as the second
# argument exists, so a test can observe startup before the handshake settles
# without sleeping for a guess at a duration. Nothing here touches the network.
set -euo pipefail

mode="${1:-ok}"
scratch="${2:-}"

respond() {
    printf '%s\n' "$1"
}

# Echo the environment a server actually received, one NAME=value per line.
dump_env() {
    local out=""
    for name in T8FUL_TEST_MCP_TOKEN T8FUL_TEST_OTHER_TOKEN HOME; do
        if [ -n "${!name+set}" ]; then
            out+="${name}=${!name}"$'\n'
        fi
    done
    printf '%s' "$out"
}

tools_list() {
    case "$mode" in
    malformed-schema)
        # `inputSchema` is required by the protocol; one entry omits it and one
        # sends a non-object, which `rmcp` rejects for the whole list. The
        # client must report that as a server failure rather than dropping tools
        # silently.
        respond '{"jsonrpc":"2.0","id":'"$1"',"result":{"tools":[{"name":"broken","description":"schema is a string","inputSchema":"not-an-object"},{"name":"odd","inputSchema":{"type":"object"}}]}}'
        ;;
    *)
        respond '{"jsonrpc":"2.0","id":'"$1"',"result":{"tools":[{"name":"echo","description":"Echo the text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},{"name":"env","description":"Report the environment this server received","inputSchema":{"type":"object","properties":{}}},{"name":"fail","description":"Always fails","inputSchema":{"type":"object","properties":{}}},{"name":"huge","description":"Returns more text than the output limit","inputSchema":{"type":"object","properties":{}}}]}}'
        ;;
    esac
}

call_tool() {
    local id="$1"
    local name="$2"
    case "$name" in
    echo)
        respond '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"echoed"}],"isError":false}}'
        ;;
    env)
        local payload
        payload="$(dump_env | sed ':a;N;$!ba;s/\n/\\n/g')"
        respond '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"'"$payload"'"}],"isError":false}}'
        ;;
    fail)
        respond '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"deliberate failure"}],"isError":true}}'
        ;;
    huge)
        # 4 KiB of text, well past a small shell.max_output_bytes.
        local blob
        blob="$(printf 'a%.0s' $(seq 1 4096))"
        respond '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"'"$blob"'"}],"isError":false}}'
        ;;
    broken|odd)
        respond '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}'
        ;;
    dies)
        # Exit without answering: the caller must report a transport failure
        # rather than hanging or replaying the call.
        exit 1
        ;;
    *)
        respond '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32601,"message":"unknown tool"}}'
        ;;
    esac
}

# Count `tools/call` requests, so a test can prove the adapter sends exactly one
# request per execution and never retries on its own.
if [ -n "${T8FUL_FAKE_CALL_LOG:-}" ]; then
    : >"$T8FUL_FAKE_CALL_LOG" 2>/dev/null || true
fi

count_call() {
    if [ -n "${T8FUL_FAKE_CALL_LOG:-}" ]; then
        printf '%s\n' "$1" >>"$T8FUL_FAKE_CALL_LOG" 2>/dev/null || true
    fi
}

# Record every request method in arrival order, so a test can prove the client
# probed `server/discover` before falling back to `initialize`. Truncated here,
# before the read loop, so the log holds this run only.
if [ -n "${T8FUL_FAKE_METHOD_LOG:-}" ]; then
    : >"$T8FUL_FAKE_METHOD_LOG" 2>/dev/null || true
fi

count_method() {
    if [ -n "${T8FUL_FAKE_METHOD_LOG:-}" ]; then
        printf '%s\n' "$1" >>"$T8FUL_FAKE_METHOD_LOG" 2>/dev/null || true
    fi
}

if [ -n "$scratch" ] && [ "$mode" != "slow" ]; then
    # Record where the process ran, so a test can tell a sandboxed server from
    # a host one: the sandbox refuses this write outside its private /tmp.
    # In `slow` mode the argument is the handshake gate instead, so it is not
    # written here.
    if ! printf 'host\n' >"$scratch" 2>/dev/null; then
        : # The write was refused, which is what a sandboxed run looks like.
    fi
fi

while IFS= read -r line; do
    [ -z "$line" ] && continue
    method="$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')"
    id="$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')"
    [ -n "$method" ] && count_method "$method"
    case "$method" in
    initialize)
        if [ "$mode" = "slow" ] && [ -n "$scratch" ]; then
            # Deterministic gate: the test creates the file when the client is
            # allowed to finish the handshake. Without a path `slow` behaves
            # like `ok`, so a mistyped argument cannot hang the suite.
            while [ ! -e "$scratch" ]; do sleep 0.02; done
        fi
        respond '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake-mcp","version":"0.1.0"}}}'
        ;;
    notifications/initialized)
        ;;
    tools/list)
        tools_list "$id"
        ;;
    tools/call)
        name="$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')"
        count_call "$name"
        call_tool "$id" "$name"
        ;;
    *)
        respond '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32601,"message":"method not found"}}'
        ;;
    esac
done
