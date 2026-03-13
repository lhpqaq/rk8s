#!/usr/bin/env bash
# =============================================================================
# test_next_auth_e2e.sh
#
# Next.js 认证委托端到端测试脚本
# 模拟完整的 rkforge → Distribution → Next 认证流程
#
# 使用方法:
#   chmod +x test_next_auth_e2e.sh
#   ./test_next_auth_e2e.sh
#
# 前置条件:
#   - 已安装 curl 和 jq
#   - 已安装 Python3 (用于启动 mock Next 服务)
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

PASS_COUNT=0
FAIL_COUNT=0
MOCK_PID=""
CALLBACK_PID=""

cleanup() {
    echo ""
    echo -e "${CYAN}=== 清理测试资源 ===${NC}"
    if [ -n "$MOCK_PID" ] && kill -0 "$MOCK_PID" 2>/dev/null; then
        kill "$MOCK_PID" 2>/dev/null || true
        echo "  已停止 Mock Next 服务 (PID: $MOCK_PID)"
    fi
    if [ -n "$CALLBACK_PID" ] && kill -0 "$CALLBACK_PID" 2>/dev/null; then
        kill "$CALLBACK_PID" 2>/dev/null || true
        echo "  已停止回调服务器 (PID: $CALLBACK_PID)"
    fi
    rm -f /tmp/mock_next_server.py /tmp/callback_server.py /tmp/callback_token.txt
}
trap cleanup EXIT

pass() {
    echo -e "  ${GREEN}✓ PASS${NC}: $1"
    PASS_COUNT=$((PASS_COUNT + 1))
}

fail() {
    echo -e "  ${RED}✗ FAIL${NC}: $1"
    FAIL_COUNT=$((FAIL_COUNT + 1))
}

echo -e "${CYAN}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   Next.js 认证委托 - 端到端测试                       ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════════════╝${NC}"
echo ""

# ---------- 检查依赖 ----------
echo -e "${YELLOW}检查依赖...${NC}"
for cmd in curl jq python3; do
    if ! command -v "$cmd" &>/dev/null; then
        echo -e "${RED}错误: 需要 $cmd 但未安装${NC}"
        exit 1
    fi
done
echo -e "${GREEN}依赖检查通过${NC}"
echo ""

# ---------- 启动 Mock Next 服务 ----------
MOCK_PORT=18900

cat > /tmp/mock_next_server.py << 'PYEOF'
"""Mock Next.js auth program for testing."""
import http.server
import json
import sys

PORT = int(sys.argv[1])

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/api/auth/verify" or self.path.startswith("/api/auth/verify?"):
            auth = self.headers.get("Authorization", "")
            if auth == "Bearer valid-next-token":
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps({"username": "testuser"}).encode())
            else:
                self.send_response(401)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps({"error": "unauthorized"}).encode())
        elif self.path.startswith("/auth/login"):
            # 模拟登录页面
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            self.wfile.write(b"<h1>Mock Next Login Page</h1>")
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        pass  # 静默日志

httpd = http.server.HTTPServer(("127.0.0.1", PORT), Handler)
print(f"Mock Next server on port {PORT}", flush=True)
httpd.serve_forever()
PYEOF

echo -e "${YELLOW}启动 Mock Next 服务 (端口 $MOCK_PORT)...${NC}"
python3 /tmp/mock_next_server.py $MOCK_PORT &
MOCK_PID=$!
sleep 1

if ! kill -0 "$MOCK_PID" 2>/dev/null; then
    echo -e "${RED}Mock Next 服务启动失败${NC}"
    exit 1
fi
echo -e "${GREEN}Mock Next 服务已启动 (PID: $MOCK_PID)${NC}"
echo ""

NEXT_URL="http://127.0.0.1:$MOCK_PORT"

# =============================================================================
# 测试 1: Token 验证 - 有效 token
# =============================================================================
echo -e "${CYAN}=== 测试 1: Bearer Token 验证 (有效 token) ===${NC}"
RESP=$(curl -s -w "\n%{http_code}" \
    -H "Authorization: Bearer valid-next-token" \
    "$NEXT_URL/api/auth/verify")
HTTP_CODE=$(echo "$RESP" | tail -1)
BODY=$(echo "$RESP" | head -1)

if [ "$HTTP_CODE" = "200" ]; then
    USERNAME=$(echo "$BODY" | jq -r '.username')
    if [ "$USERNAME" = "testuser" ]; then
        pass "有效 token 返回 200, username=$USERNAME"
    else
        fail "返回 200 但 username 不正确: $USERNAME"
    fi
else
    fail "期望 200, 实际 $HTTP_CODE"
fi

# =============================================================================
# 测试 2: Token 验证 - 无效 token
# =============================================================================
echo -e "${CYAN}=== 测试 2: Bearer Token 验证 (无效 token) ===${NC}"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -H "Authorization: Bearer bad-token" \
    "$NEXT_URL/api/auth/verify")

if [ "$HTTP_CODE" = "401" ]; then
    pass "无效 token 返回 401"
else
    fail "期望 401, 实际 $HTTP_CODE"
fi

# =============================================================================
# 测试 3: Token 验证 - 无 Authorization 头
# =============================================================================
echo -e "${CYAN}=== 测试 3: Bearer Token 验证 (无 Authorization 头) ===${NC}"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    "$NEXT_URL/api/auth/verify")

if [ "$HTTP_CODE" = "401" ]; then
    pass "无 Authorization 头返回 401"
else
    fail "期望 401, 实际 $HTTP_CODE"
fi

# =============================================================================
# 测试 4: Login URL 构造
# =============================================================================
echo -e "${CYAN}=== 测试 4: Login URL 构造逻辑 ===${NC}"
CALLBACK_URL="http://127.0.0.1:19999/callback"
BASE=$(echo "$NEXT_URL" | sed 's:/*$::')
LOGIN_URL="${BASE}/auth/login?callback_url=$(python3 -c "import urllib.parse; print(urllib.parse.quote('$CALLBACK_URL'))")"

if echo "$LOGIN_URL" | grep -q "/auth/login?callback_url="; then
    pass "Login URL 构造正确: $LOGIN_URL"
else
    fail "Login URL 构造不正确: $LOGIN_URL"
fi

# =============================================================================
# 测试 5: Login 页面可访问
# =============================================================================
echo -e "${CYAN}=== 测试 5: Login 页面可访问 ===${NC}"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$LOGIN_URL")

if [ "$HTTP_CODE" = "200" ]; then
    pass "Login 页面返回 200"
else
    fail "Login 页面返回 $HTTP_CODE"
fi

# =============================================================================
# 测试 6: 回调服务器 token 接收
# =============================================================================
echo -e "${CYAN}=== 测试 6: 回调服务器接收 token ===${NC}"
CALLBACK_PORT=18901

cat > /tmp/callback_server.py << 'PYEOF'
"""Mock rkforge callback server."""
import http.server
import json
import sys
from urllib.parse import urlparse, parse_qs

PORT = int(sys.argv[1])
TOKEN_FILE = sys.argv[2]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/callback":
            params = parse_qs(parsed.query)
            token = params.get("token", [None])[0]
            if token:
                with open(TOKEN_FILE, "w") as f:
                    f.write(token)
                self.send_response(200)
                self.send_header("Content-Type", "text/html")
                self.end_headers()
                self.wfile.write(b"<h1>Login successful!</h1>")
            else:
                self.send_response(400)
                self.end_headers()
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        pass

httpd = http.server.HTTPServer(("127.0.0.1", PORT), Handler)
print(f"Callback server on port {PORT}", flush=True)
httpd.handle_request()  # 只处理一个请求就退出
PYEOF

rm -f /tmp/callback_token.txt
python3 /tmp/callback_server.py $CALLBACK_PORT /tmp/callback_token.txt &
CALLBACK_PID=$!
sleep 0.5

# 模拟 Next 回调 (浏览器重定向)
CALLBACK_RESP=$(curl -s -w "\n%{http_code}" \
    "http://127.0.0.1:$CALLBACK_PORT/callback?token=my-next-jwt-token-123")
CALLBACK_HTTP=$(echo "$CALLBACK_RESP" | tail -1)

sleep 0.5

if [ "$CALLBACK_HTTP" = "200" ]; then
    if [ -f /tmp/callback_token.txt ]; then
        RECEIVED_TOKEN=$(cat /tmp/callback_token.txt)
        if [ "$RECEIVED_TOKEN" = "my-next-jwt-token-123" ]; then
            pass "回调服务器成功接收 token: $RECEIVED_TOKEN"
        else
            fail "token 不匹配: 期望 my-next-jwt-token-123, 实际 $RECEIVED_TOKEN"
        fi
    else
        fail "token 文件未创建"
    fi
else
    fail "回调请求返回 $CALLBACK_HTTP"
fi

# =============================================================================
# 测试 7: 端到端流程模拟
# =============================================================================
echo -e "${CYAN}=== 测试 7: 端到端流程模拟 ===${NC}"
echo "  模拟流程: rkforge → Distribution(login_url) → Next(login) → callback → token → verify"

# Step 1: 构造 login URL (Distribution 的逻辑)
E2E_CALLBACK="http://127.0.0.1:18902/callback"
E2E_LOGIN_URL="${BASE}/auth/login?callback_url=$(python3 -c "import urllib.parse; print(urllib.parse.quote('$E2E_CALLBACK'))")"

# Step 2: 验证 login URL 可访问
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$E2E_LOGIN_URL")
if [ "$HTTP_CODE" != "200" ]; then
    fail "Login URL 不可访问 (HTTP $HTTP_CODE)"
else
    # Step 3: 模拟用户登录成功, 获得 token
    TOKEN="valid-next-token"

    # Step 4: 用 token 进行验证 (Distribution → Next)
    VERIFY_RESP=$(curl -s -w "\n%{http_code}" \
        -H "Authorization: Bearer $TOKEN" \
        "$NEXT_URL/api/auth/verify")
    VERIFY_HTTP=$(echo "$VERIFY_RESP" | tail -1)
    VERIFY_BODY=$(echo "$VERIFY_RESP" | head -1)

    if [ "$VERIFY_HTTP" = "200" ]; then
        VERIFY_USER=$(echo "$VERIFY_BODY" | jq -r '.username')
        if [ "$VERIFY_USER" = "testuser" ]; then
            pass "端到端流程: 认证成功, 用户名=$VERIFY_USER"
        else
            fail "端到端流程: 用户名不正确 $VERIFY_USER"
        fi
    else
        fail "端到端流程: token 验证失败 (HTTP $VERIFY_HTTP)"
    fi
fi

# =============================================================================
# 测试 8: NEXT_AUTH_URL 尾斜杠处理
# =============================================================================
echo -e "${CYAN}=== 测试 8: NEXT_AUTH_URL 尾斜杠处理 ===${NC}"
# 测试带尾斜杠的 URL 也能正常工作
NEXT_URL_SLASH="${NEXT_URL}/"
VERIFY_URL=$(echo "$NEXT_URL_SLASH" | sed 's:/*$::')/api/auth/verify
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
    -H "Authorization: Bearer valid-next-token" \
    "$VERIFY_URL")

if [ "$HTTP_CODE" = "200" ]; then
    pass "尾斜杠处理正确"
else
    fail "尾斜杠处理有问题 (HTTP $HTTP_CODE)"
fi

# =============================================================================
# 测试结果汇总
# =============================================================================
echo ""
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"
TOTAL=$((PASS_COUNT + FAIL_COUNT))
echo -e "  测试总数: $TOTAL"
echo -e "  ${GREEN}通过: $PASS_COUNT${NC}"
if [ "$FAIL_COUNT" -gt 0 ]; then
    echo -e "  ${RED}失败: $FAIL_COUNT${NC}"
fi
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"

if [ "$FAIL_COUNT" -gt 0 ]; then
    echo -e "${RED}测试未全部通过!${NC}"
    exit 1
else
    echo -e "${GREEN}所有测试通过! ✓${NC}"
    exit 0
fi
