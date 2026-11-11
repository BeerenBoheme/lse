#!/usr/bin/env bash
# lse 基准测试：
#   1) 自动生成 10 万个测试文件（可调）；
#   2) 测量全量建索引耗时（含预统计 + 遍历 + 落库）；
#   3) 测量冷启动加载耗时与查询延迟（p50/p95/max）；
#   4) 测量常驻内存（RSS/HWM）与索引体积（相对原始路径字节的倍率）。
# 用法: ./bench.sh [文件数] [目录数]
set -uo pipefail

FILES="${1:-100000}"
DIRS="${2:-200}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$ROOT/.bench/tree"
DB="$ROOT/.bench/bench.db"
BIN="$ROOT/target/release/lse-bench"
OUT="$ROOT/.bench/last-result.txt"

export CARGO_HOME="$ROOT/.toolchain/cargo"
export RUSTUP_HOME="$ROOT/.toolchain/rustup"
export PATH="$CARGO_HOME/bin:$PATH"

echo "== lse bench ==  files=$FILES dirs=$DIRS"

# 1. 构建（限并发，避免机器卡顿）
echo "-- build binaries (release, -j6)"
cargo build --release -p lse-bench -j 6 2>&1 | tail -1

# 2. 生成测试文件
echo "-- generate test files"
"$BIN" gen "$BENCH_DIR" "$FILES" "$DIRS"

# 3. 建索引 + 查询 + 内存
echo "-- run benchmark"
mkdir -p "$(dirname "$DB")"
"$BIN" run --root "$BENCH_DIR" --db "$DB" | tee "$OUT"

# 4. 汇总
echo "== summary =="
awk -F= '
/^BUILD_TOTAL_MS/   {printf "建索引总耗时      : %s ms\n", $2}
/^BUILD_SCAN_MS/    {printf "  其中遍历        : %s ms\n", $2}
/^BUILD_DB_MS/      {printf "  其中写库        : %s ms\n", $2}
/^LOAD_MS/          {printf "冷启动加载        : %s ms\n", $2}
/^QUERY_report_P50/ {printf "查询 p50 (report) : %s µs\n", $2}
/^QUERY_report_P95/ {printf "查询 p95 (report) : %s µs\n", $2}
/^QUERY_z_P50/      {printf "查询 p50 (低频 z)  : %s µs\n", $2}
/^DB_RATIO/         {printf "索引体积/路径字节 : %s\n", $2}
/^RSS_KB/           {printf "常驻内存 RSS      : %.1f MB\n", $2/1024}
/^HWM_KB/           {printf "内存峰值 HWM      : %.1f MB\n", $2/1024}
/^DIRS/             {printf "索引目录数        : %s\n", $2}
/^FILES/            {printf "索引文件数        : %s\n", $2}
' "$OUT"

# 5. 保留原始输出
echo "结果已保存: $OUT"
