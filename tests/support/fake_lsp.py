#!/usr/bin/env python3
"""A scriptable language server for tests that need deterministic server
behaviour (a hover that hangs, a request the server never answers, ...)
rather than whatever a real server happens to do.

Speaks just enough LSP over stdio: initialize/initialized, didOpen/didChange
(tracking document text), documentSymbol (one symbol per `fn <name>` line),
hover, shutdown/exit. Behaviour is controlled through environment variables,
which reach it from the test via CLI -> daemon -> server inheritance:

  FAKE_LSP_HOVER_DELAY   seconds to sleep before answering a hover on any
                         line but the first (line 0 stays fast so the
                         daemon's readiness probe isn't affected)
  FAKE_LSP_MARKER_DIR    directory to touch `hover-started` in when a slow
                         hover begins, so a test can wait for it instead of
                         guessing with sleeps
  FAKE_LSP_LOG           file to append every received method name to
  FAKE_LSP_FLAT_SYMBOLS  if set, answer documentSymbol with the flat
                         SymbolInformation[] shape instead of DocumentSymbol[]

Call hierarchy: prepareCallHierarchy returns the symbol under the cursor
with an opaque `data` token, and incomingCalls answers only if that token
comes back intact (as a real server that keys state on `data` would);
otherwise it returns an error.
"""

import json
import os
import re
import sys
import time

if len(sys.argv) > 1 and sys.argv[1] in ("--version", "version"):
    print("fake-lsp 1.0.0")
    sys.exit(0)

stdin = sys.stdin.buffer
stdout = sys.stdout.buffer
docs = {}


def read_message():
    length = None
    while True:
        line = stdin.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        name, _, value = line.decode().partition(":")
        if name.lower() == "content-length":
            length = int(value.strip())
    if length is None:
        return None
    return json.loads(stdin.read(length))


def send(msg):
    body = json.dumps(msg).encode()
    stdout.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    stdout.flush()


def log(method):
    path = os.environ.get("FAKE_LSP_LOG")
    if path:
        with open(path, "a") as f:
            f.write(method + "\n")


def symbols(text):
    out = []
    for line_no, line in enumerate(text.split("\n")):
        m = re.search(r"\bfn\s+(\w+)", line)
        if m:
            start, end = m.start(1), m.end(1)
            rng = {
                "start": {"line": line_no, "character": 0},
                "end": {"line": line_no, "character": len(line)},
            }
            sel = {
                "start": {"line": line_no, "character": start},
                "end": {"line": line_no, "character": end},
            }
            out.append(
                {"name": m.group(1), "kind": 12, "range": rng, "selectionRange": sel}
            )
    return out


while True:
    msg = read_message()
    if msg is None:
        break
    method = msg.get("method")
    if method:
        log(method)
    params = msg.get("params") or {}
    if "id" not in msg:
        if method == "textDocument/didOpen":
            td = params["textDocument"]
            docs[td["uri"]] = td["text"]
        elif method == "textDocument/didChange":
            docs[params["textDocument"]["uri"]] = params["contentChanges"][-1]["text"]
        elif method == "exit":
            break
        continue

    result = None
    if method == "initialize":
        result = {
            "capabilities": {
                "textDocumentSync": 1,
                "documentSymbolProvider": True,
                "hoverProvider": True,
            },
            "serverInfo": {"name": "fake-lsp"},
        }
    elif method == "textDocument/documentSymbol":
        uri = params["textDocument"]["uri"]
        result = symbols(docs.get(uri, ""))
        if os.environ.get("FAKE_LSP_FLAT_SYMBOLS"):
            result = [
                {"name": s["name"], "kind": s["kind"],
                 "location": {"uri": uri, "range": s["range"]}}
                for s in result
            ]
    elif method == "workspace/symbol":
        query = params.get("query", "")
        result = [
            {"name": s["name"], "kind": s["kind"],
             "location": {"uri": uri, "range": s["range"]},
             "containerName": "from-fake-lsp"}
            for uri, text in docs.items()
            for s in symbols(text)
            if query in s["name"]
        ]
    elif method == "textDocument/prepareCallHierarchy":
        uri = params["textDocument"]["uri"]
        line = params["position"]["line"]
        result = [
            dict(s, uri=uri, data={"token": "opaque-%d" % line})
            for s in symbols(docs.get(uri, ""))
            if s["range"]["start"]["line"] == line
        ]
    elif method == "callHierarchy/incomingCalls":
        item = params["item"]
        if item.get("data", {}).get("token", "").startswith("opaque-"):
            caller = dict(item, name="caller_of_" + item["name"])
            caller.pop("data", None)
            result = [{"from": caller, "fromRanges": [item["selectionRange"]]}]
        else:
            send({"jsonrpc": "2.0", "id": msg["id"],
                  "error": {"code": -32602, "message": "item data missing"}})
            continue
    elif method == "textDocument/hover":
        line = params["position"]["line"]
        if line > 0:
            marker_dir = os.environ.get("FAKE_LSP_MARKER_DIR")
            if marker_dir:
                open(os.path.join(marker_dir, "hover-started"), "w").close()
            time.sleep(float(os.environ.get("FAKE_LSP_HOVER_DELAY", "0")))
        text = docs.get(params["textDocument"]["uri"], "").split("\n")
        word = text[line].strip() if line < len(text) else ""
        result = {"contents": {"kind": "plaintext", "value": "hover: " + word}}
    send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
