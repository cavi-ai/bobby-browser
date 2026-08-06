# Bobby Vision Assist - Local MLX/Ollama Setup

## Overview

This directory contains tools for running vision-language models locally on
Apple Silicon for Bobby Browser's vision assist feature.

## Quick Start

### Option 1: Ollama (Recommended - Working Now)

Ollama with llava:7b provides working vision assist on your M5 Max:

```bash
# Start the Ollama vision provider (normalizes output to Bobby's schema)
python3 scripts/vision-mlx/ollama_vision.py --bind 127.0.0.1:9103

# Or configure Bobby to use Ollama directly:
bobby vision connect --provider ollama --yes
```

**Requirements:**
- Ollama installed (`ollama pull llava:7b`)
- Model loaded in Ollama (`ollama ps`)

**Configuration:**
| Env Var | Default | Description |
|---|---|---|
| `VISION_OLLAMA_MODEL` | `llava:7b` | Ollama model to use |
| `VISION_OLLAMA_BIND` | `127.0.0.1:9103` | Bind address |
| `VISION_OLLAMA_BASE_URL` | `http://127.0.0.1:11434` | Ollama API URL |

### Option 2: Pure MLX (Future - When Vision Support Lands)

When MLX adds vision model support, use the pure MLX pipeline:

```bash
# Install dependencies
pip3 install -r scripts/vision-mlx/requirements.txt

# Start the MLX vision service
python3 scripts/vision-mlx/vision_mlx.py --model qwen2-vl:7b --bind 127.0.0.1:9101
```

**Requirements:**
- MLX with vision support (not yet available in MLX 0.31.3)
- PyTorch + transformers for vision encoder fallback
- ~14GB RAM for 7B model (quantized)

**Configuration:**
| Env Var | Default | Description |
|---|---|---|
| `VISION_MLX_MODEL` | `qwen2-vl:7b` | Model to load |
| `VISION_MLX_BIND` | `127.0.0.1:9101` | Bind address |
| `VISION_MLX_PRELOAD` | `false` | Preload model at startup |

## Bobby Integration

### CLI Configuration

Configure Bobby to use Ollama:

```bash
# Direct provider (recommended)
bobby vision connect --provider ollama --yes

# Custom Ollama endpoint
bobby vision connect --provider custom \
    --base-url http://127.0.0.1:11434 \
    --model llava:7b \
    --yes
```

### Manual Proxy

Run the vision proxy with Ollama upstream:

```bash
bobby vision-proxy --ollama --model llava:7b \
    --ollama-base-url http://127.0.0.1:11434 \
    --bind 127.0.0.1:9100
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ Bobby Browser Runtime                                       │
│                                                             │
│  Intent Engine → HttpVisionAssist → POST /propose          │
│                    (screenshot + context)                    │
└──────────────────────┬──────────────────────────────────────┘
                       │ HTTP
                       ▼
┌─────────────────────────────────────────────────────────────┐
│ Vision Provider (Local)                                     │
│                                                             │
│  ┌──────────────┐     ┌──────────────────┐                 │
│  │ Ollama       │     │ MLX (Future)     │                 │
│  │ llava:7b     │     │ Qwen2-VL 7B      │                 │
│  │ (working)    │     │ (when MLX adds   │                 │
│  │              │     │  vision support)  │                 │
│  └──────────────┘     └──────────────────┘                 │
│       │                       │                            │
│       ▼                       ▼                            │
│  Normalized to Bobby's      MLX text decoder             │
│  VisionProposal schema      + PyTorch vision encoder     │
└─────────────────────────────────────────────────────────────┘
```

## Model Comparison

| Model | Speed | Accuracy | RAM | Status |
|---|---|---|---|---|
| llava:7b (Ollama) | ~2s | Medium | ~5GB | ✅ Working |
| Qwen2-VL 7B (MLX) | ~1s | High | ~14GB | ⏳ Waiting |
| gpt-4o (Cloud) | ~0.5s | High | N/A | ✅ Available |

## Training Data Collection

To fine-tune a model for Bobby's specific use case:

1. Run gauntlet tests with vision assist enabled
2. Capture all vision proposals (input + output + outcome)
3. Store as JSONL: `{image_b64, prompt, response, success, metadata}`
4. Fine-tune with LoRA on the captured trajectories

## Troubleshooting

**Ollama not responding:**
```bash
ollama ps  # Check if model is loaded
ollama pull llava:7b  # Pull if missing
```

**Vision proxy not reachable:**
```bash
curl http://127.0.0.1:9100/propose  # Test endpoint
bobby doctor  # Check vision route
```

**Model returns wrong schema:**
- Check the prompt in `ollama_vision.py` is being used
- Ensure Ollama model is llava:7b or similar vision model
