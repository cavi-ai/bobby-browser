# Bobby Vision Assist - Canonical Provider Interface

## Overview

Bobby's vision assist uses a canonical provider interface. One interface,
swappable backends — direct local MLX inference, Ollama, or LM Studio.
The output schema matches the Rust `vision-proxy` wire format exactly.

## Architecture

```
Bobby runtime → vision-proxy (Rust) → POST /propose
                      │
                      ▼
              Canonical VisionProvider
              ┌───────┬────────┬──────────┐
              │mlx-vlm│ ollama │ lmstudio │
              └───────┴────────┴──────────┘
```

`providers/base.py` defines the interface — `propose()` and `extract()` —
mirroring the Rust `VisionAssist` trait and `wire.rs` schema:

```json
{"confidence": 0.0..1.0, "action": {"kind": "click"|"typeText"|"extractValue", ...}}
```

## Providers

| Provider | Backend | Speed (3B) | Setup |
|---|---|---|---|
| `mlx-vlm` (default) | Direct local inference via mlx-vlm on Metal | ~0.55s | `pip install mlx-vlm`, no server |
| `ollama` | HTTP to local Ollama | ~2s | `ollama pull llava:7b` |
| `lmstudio` | HTTP to LM Studio | ~1s | LM Studio server on :1234 |

## Usage

### Direct (Python)

```python
from providers import create_provider, ProposeRequest

provider = create_provider("mlx-vlm")          # or "ollama", "lmstudio"
resp = provider.propose(ProposeRequest(
    purpose="Click OK button",
    intent_kind="locate",
    stuck="targetMissing",
    screenshot_b64=png_b64,
))
# -> {"confidence": 0.95, "action": {"kind": "click", "x": 199.0, "y": 101.0}}
```

### Server

```bash
python vision_server.py --provider mlx-vlm --bind 127.0.0.1:9101
VISION_PROVIDER=ollama python vision_server.py
```

### Configuration (env)

| Env var | Default | Description |
|---|---|---|
| `VISION_PROVIDER` | `mlx-vlm` | Backend selection |
| `VISION_MLX_MODEL` | `mlx-community/Qwen2.5-VL-3B-Instruct-4bit` | mlx-vlm model |
| `VISION_OLLAMA_MODEL` | `llava:7b` | Ollama model |
| `VISION_OLLAMA_BASE_URL` | `http://127.0.0.1:11434` | Ollama URL |
| `VISION_LMSTUDIO_MODEL` | `local-model` | LM Studio model |
| `VISION_LMSTUDIO_BASE_URL` | `http://127.0.0.1:1234` | LM Studio URL |

### Evaluation

```bash
python eval_provider.py --provider mlx-vlm --input data/training_data.jsonl --limit 30
```

Latest results (Qwen2.5-VL-3B, synthetic data): 90% action accuracy, 0.55s avg latency.

## Model response normalization

Models emit coordinates variously — `{"x": N, "y": N}`, `{"coordinate": [N, N]}`,
`{"position": {...}}`, or bare `"action": "click"` with sibling coordinate fields.
`normalize_action()` maps all of these onto the canonical schema; unknown
variants degrade to `{"kind": "click", "x": 0.0, "y": 0.0}` rather than failing.

## Training data

Collect with `bobby serve --vision --collect-training-data` (Rust) or generate
synthetic data with `bobby_vision_collector.py --generate --num-examples 1000`.
