#!/usr/bin/env python3
"""刷新内置的 models.dev 元数据快照 assets/models.json.gz。

从 https://models.dev/api.json 拉全量，只留 carter 用到的字段，gzip 后写入。
发版前跑一次，让离线兜底不至于太旧：

    python3 scripts/refresh-models.py [--proxy http://127.0.0.1:7897]
"""
import argparse, datetime, gzip, json, os, sys, urllib.request

KEEP_PROVIDER = ("id", "name", "doc", "env", "api")
KEEP_MODEL = ("id", "name", "family", "attachment", "reasoning", "tool_call",
              "reasoning_options", "modalities", "limit", "cost", "release_date",
              "canonical_model_id", "status")

ap = argparse.ArgumentParser()
ap.add_argument("--proxy")
ap.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "..", "assets", "models.json.gz"))
args = ap.parse_args()

if args.proxy:
    urllib.request.install_opener(urllib.request.build_opener(
        urllib.request.ProxyHandler({"http": args.proxy, "https": args.proxy})))
req = urllib.request.Request("https://models.dev/api.json", headers={"User-Agent": "carter-refresh-models"})
raw = urllib.request.urlopen(req, timeout=60).read()
full = json.loads(raw)

slim = {}
for pid, p in full.items():
    slim[pid] = {k: p[k] for k in KEEP_PROVIDER if k in p}
    slim[pid]["models"] = {mid: {k: m[k] for k in KEEP_MODEL if k in m}
                           for mid, m in p.get("models", {}).items()}
# 生成日期：界面要告诉用户「你用的是哪天的目录」，否则没法判断该不该联网刷新。
# 以下划线开头，不会被当成一个 provider。
slim["$generated_at"] = datetime.date.today().isoformat()

data = json.dumps(slim, ensure_ascii=False, separators=(",", ":")).encode()
os.makedirs(os.path.dirname(args.out), exist_ok=True)
with open(args.out, "wb") as f:
    f.write(gzip.compress(data, 9))
n_models = sum(len(p["models"]) for p in slim.values() if isinstance(p, dict))
print(f"{len(slim) - 1} providers / {n_models} models → {args.out} ({os.path.getsize(args.out)} bytes)", file=sys.stderr)
