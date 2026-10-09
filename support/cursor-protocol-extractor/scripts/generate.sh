#!/usr/bin/env bash
# generate.sh 根据提取的 Proto 定义生成可供其他 Go module 使用的消息包。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPOSITORY_DIR="$(cd "$PROJECT_DIR/../.." && pwd)"
PROTO_DIR="$REPOSITORY_DIR/protocols/cursor"
MODULE_PATH="github.com/leookun/cursor-byok/cursor-proto"

command -v protoc >/dev/null 2>&1 || {
  echo "protoc is required" >&2
  exit 1
}
command -v protoc-gen-go >/dev/null 2>&1 || {
  echo "protoc-gen-go is required" >&2
  exit 1
}

# 协议声明使用 proto3 optional(约 6.5k 个字段)。protoc 在 3.12-3.14 需要显式
# 传入 --experimental_allow_proto3_optional,3.15 起才默认开启。旧版本会直接以
# "This file contains proto3 optional fields, but --experimental_allow_proto3_optional
# was not set" 中断,这里提前给出可操作的提示。
PROTOC_VERSION="$(protoc --version | awk '{print $NF}')"
PROTOC_MAJOR="${PROTOC_VERSION%%.*}"
PROTOC_MINOR="${PROTOC_VERSION#*.}"
PROTOC_MINOR="${PROTOC_MINOR%%.*}"
PROTOC_OPTIONAL_READY=true
if [[ "$PROTOC_MAJOR" =~ ^[0-9]+$ ]] && [[ "$PROTOC_MINOR" =~ ^[0-9]+$ ]]; then
  if (( PROTOC_MAJOR < 3 )); then
    PROTOC_OPTIONAL_READY=false
  elif (( PROTOC_MAJOR == 3 && PROTOC_MINOR < 15 )); then
    PROTOC_OPTIONAL_READY=false
  fi
fi
if [[ "$PROTOC_OPTIONAL_READY" = false ]]; then
  echo "protoc 3.15+ is required (found libprotoc $PROTOC_VERSION):" >&2
  echo "the extracted schema uses proto3 optional fields." >&2
  echo "CI pins 31.1 via arduino/setup-protoc." >&2
  exit 1
fi

for PROTO_FILE in agent_v1.proto aiserver_v1.proto; do
  if [[ ! -f "$PROTO_DIR/$PROTO_FILE" ]]; then
    echo "Missing Proto source: $PROTO_DIR/$PROTO_FILE" >&2
    exit 1
  fi
done

protoc \
  --proto_path="$PROTO_DIR" \
  --go_out="$PROJECT_DIR" \
  --go_opt="module=$MODULE_PATH" \
  "$PROTO_DIR/agent_v1.proto" \
  "$PROTO_DIR/aiserver_v1.proto"

echo "Generated Go packages under: $PROJECT_DIR/gen"
