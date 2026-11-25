# Jev synthetic icon grounding

Jev 1.14.0 selected the correct opaque candidate ID for **17/22 present targets** (77.3%) with the PNG and **0/22** with the same text alone. Both arms abstained on **11/11 missing targets**. All five image misses were abstentions on the filled-square stop icon. Neither arm chose an incorrect candidate ID. The text-only arm abstained on every question, so its missing-target score alone gives no evidence of visual discrimination.

## Protocol

On 2026-11-25, each of 11 fixed seeds (101–111) generated a 320 × 320 white RGB board with four different generic black pictograms sampled from search, play, pause, stop and plus controls and a home icon. Each icon had one opaque A–D ID below it. The harness shuffled icons and IDs independently and jittered their positions within four quadrants. Each board supplied three shuffled Choice questions in one request, two for icons present and one for an absent icon. Ground truth stayed local. The model could return an ID or abstain. The text-only arm used identical candidate IDs, questions, criteria and model, omitting the board and its asset. Arm order alternated by seed. Each request was independent.

The official OpenAPI schema specifies POST /v1/systemone with a SystemOneRequest JSON body. The image arm references an entry in the top-level assets map from state.board. Its type is image, format is png and data_base64 contains the PNG. Both arms send the image opt-in header. TypeSafe documents PNG and JPEG support and describes image handling as experimental. [OpenAPI schema](https://api.typesafe.ai/openapi.json) · [Image format and limits](https://docs.typesafe.ai/images.md)

## Receipts and timing

| Arm | Requests | Median first body byte | Median full body | Input tokens | Output tokens | Estimated USD |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Image | 11 | 168.57 ms | 168.60 ms | 10,340 | 1,683 | 0.00043428 |
| Text only | 11 | 167.93 ms | 168.01 ms | 6,820 | 1,683 | 0.00028644 |

Each duration starts just before sending on a fresh HTTPS connection and includes connection setup and upload. First-body time ends after the client reads one response body byte. Total time ends when the client finishes reading the JSON body, before parsing. These are request-level timings for three batched decisions. The endpoint returned one JSON document, so token generation latency was unobservable.

The 22 scored requests used 17,160 input and 3,366 output tokens, for **USD 0.00072072** at the published USD 0.042 per million input tokens and free output. One preliminary 256 × 256 image request received HTTP 400. The documented minimum is 280 × 280. Its usage and cost are unknown. The whole experiment made **23 live POSTs**, including that pilot. [Model pricing](https://docs.typesafe.ai/models.md)

The harness generates every pixel locally. It has no device interface, screenshot input, image path argument or action executor. Only the built-in fixture and task content can enter a request. The sample covers one drawing style, six icons, one grid density and a single host and time window. Questions on the same board share visual context. The results establish candidate-ID grounding on these fixtures only. Device coordinates, accessibility trees, taps, actual screen recognition and end-to-end latency were outside this experiment.

## Reproduction

From the repository root with Python 3 and Pillow (recorded with Pillow 13.0.0), use the default offline mode to regenerate the fixture hashes and expected decisions. Each successful request in receipts.jsonl records its arm, fixture hash, expected and observed choices, response model, status, token counts and timings. pilot.jsonl records the rejected attempt.

```sh
python experimental/jev-vision/benchmark.py
```

A new live replication requires an existing TYPESAFE_API_KEY in the environment and an unused output path. It makes 22 POSTs, with no automatic retries. The recorded experiment is complete.

```sh
python experimental/jev-vision/benchmark.py --live new-receipts.jsonl
```
