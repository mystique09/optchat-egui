"""Local protocol fixture; no external services or credentials."""
import json
import sys
import threading
import time
import hashlib
import base64
from urllib.parse import urlparse, parse_qs, urlencode
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

mode, log_path, label = sys.argv[1:4]
lock = threading.Lock()
authorization = {}


def respond(message):
    with lock:
        with open(log_path, "a") as log:
            log.write(json.dumps(message) + "\n")
    method = message.get("method")
    if "id" not in message:
        return None
    if method == "initialize":
        result = {"protocolVersion": "2025-11-25",
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fixture", "version": "1"}}
    elif method == "tools/list":
        if message.get("params", {}).get("cursor") == "second":
            result = {"tools": [{"name": "fail", "inputSchema": {"type": "object"}}]}
        else:
            result = {"tools": [{"name": "echo", "description": "fixture echo",
                                 "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}}}}],
                      "nextCursor": "second"}
    elif method == "tools/call":
        args = message["params"].get("arguments", {})
        if args.get("value") == "stall":
            time.sleep(5)
        result = {"content": [{"type": "text", "text": label + ":" + args.get("value", "")},
                              {"type": "image", "data": "AA==", "mimeType": "image/png"}],
                  "structuredContent": {"label": label, "arguments": args},
                  "isError": message["params"]["name"] == "fail"}
    else:
        return {"jsonrpc": "2.0", "id": message["id"],
                "error": {"code": -32601, "message": "Method not found"}}
    return {"jsonrpc": "2.0", "id": message["id"], "result": result}


if mode == "stdio":
    def handle(line):
        result = respond(json.loads(line))
        if result is not None:
            with lock:
                print(json.dumps(result), flush=True)
    for line in sys.stdin:
        threading.Thread(target=handle, args=(line,), daemon=True).start()
else:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            if mode == "oauth":
                base = f"http://127.0.0.1:{self.server.server_port}"
                path = urlparse(self.path)
                if path.path.startswith("/.well-known/oauth-protected-resource"):
                    return self.json_response({"resource": base + "/mcp", "authorization_servers": [base], "scopes_supported": ["read"]})
                if path.path == "/.well-known/oauth-authorization-server":
                    return self.json_response({"issuer": base, "authorization_endpoint": base + "/authorize", "token_endpoint": base + "/token", "registration_endpoint": base + "/register", "response_types_supported": ["code"], "grant_types_supported": ["authorization_code", "refresh_token"], "code_challenge_methods_supported": ["S256"], "token_endpoint_auth_methods_supported": ["none"], "scopes_supported": ["read"]})
                if path.path == "/authorize":
                    args = parse_qs(path.query)
                    assert args["code_challenge_method"] == ["S256"]
                    assert args["resource"] == [base + "/mcp"]
                    authorization.update(args)
                    result = {"code": "fixture-code", "state": args["state"][0], "iss": base}
                    self.send_response(302)
                    self.send_header("Location", args["redirect_uri"][0] + "?" + urlencode(result))
                    self.end_headers()
                    return
                if path.path == "/mcp" and not self.authorized():
                    return self.challenge()
            self.send_response(405)
            self.end_headers()

        def json_response(self, value):
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def authorized(self):
            return self.headers.get("Authorization") == "Bearer refreshed-token"

        def challenge(self):
            self.send_response(401)
            self.send_header("WWW-Authenticate", f'Bearer resource_metadata="http://127.0.0.1:{self.server.server_port}/.well-known/oauth-protected-resource/mcp", scope="read"')
            self.end_headers()

        def do_DELETE(self):
            self.send_response(200)
            self.end_headers()

        def do_POST(self):
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            if mode == "oauth":
                if self.path == "/register":
                    request = json.loads(raw)
                    assert request["token_endpoint_auth_method"] == "none"
                    return self.json_response({"client_id": "fixture-client", "redirect_uris": request["redirect_uris"], "token_endpoint_auth_method": "none"})
                if self.path == "/token":
                    request = parse_qs(raw.decode())
                    assert request["resource"] == [f"http://127.0.0.1:{self.server.server_port}/mcp"]
                    refresh = request["grant_type"] == ["refresh_token"]
                    if not refresh:
                        assert request["code"] == ["fixture-code"]
                        challenge = base64.urlsafe_b64encode(hashlib.sha256(request["code_verifier"][0].encode()).digest()).rstrip(b"=").decode()
                        assert challenge == authorization["code_challenge"][0]
                    else:
                        assert request["refresh_token"] == ["fixture-refresh"]
                    with open(log_path, "a") as log:
                        log.write(json.dumps({"oauth": "refresh" if refresh else "exchange"}) + "\n")
                    return self.json_response({"access_token": "refreshed-token" if refresh else "expired-token", "refresh_token": "fixture-refresh", "token_type": "Bearer", "expires_in": 3600 if refresh else 0, "scope": "read"})
                if not self.authorized():
                    return self.challenge()
            message = json.loads(raw)
            if mode == "model":
                if message["model"] == "fixture-compactor":
                    block = {"type": "text", "text": "user: test integration; echo: local fixture result."}
                    stop = "end_turn"
                elif len(message["messages"]) == 1:
                    tool = next(tool for tool in message["tools"] if "tool echo." in tool.get("description", ""))
                    block = {"type": "tool_use", "id": "fixture-call", "name": tool["name"], "input": {"value": "UI verification"}}
                    stop = "tool_use"
                else:
                    block = {"type": "text", "text": "Local fixture completed; the tool result was received."}
                    stop = "end_turn"
                events = [{"type": "message_start", "message": {"usage": {"input_tokens": 1}}},
                          {"type": "content_block_start", "index": 0, "content_block": block},
                          {"type": "content_block_stop", "index": 0},
                          {"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 1}},
                          {"type": "message_stop"}]
                body = "".join("data: " + json.dumps(event) + "\n\n" for event in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            result = respond(message)
            if result is None:
                self.send_response(202)
                self.end_headers()
                return
            body = json.dumps(result)
            if mode == "sse":
                body = "event: message\ndata: " + body + "\n\n"
            body = body.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if mode == "sse" else "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_port, flush=True)
    server.serve_forever()
