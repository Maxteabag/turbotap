# Conditional raw JPEG experiment

Apply the base and then the overlay in a clean scratch WDA checkout. Run from the turbotap root, with WDA set to that checkout at d17782422d55ff1e5e0ceb74eb1fd509cc0c35b6.

```sh
lab="$PWD/experiments/conditional-screenshot"
git -C "$WDA" apply "$PWD/patches/wda-turbo.patch"
git -C "$WDA" apply --check "$lab/wda.patch"
git -C "$WDA" apply "$lab/wda.patch"
```

The overlay modifies FBScreenshotCommands.m only. Existing raw and base64 requests keep their wire format. For POST /wda/fastScreenshot, send raw=true and jpegProbe="conditional". The first response carries the whole JPEG and a quoted SHA-256 ETag. Send the entire returned header value (including quotes) as the next request's JSON previousETag. Every request captures and scales before comparing. A match returns HTTP 200, the same ETag and zero body bytes. A changed frame returns HTTP 200 and the full new JPEG. This is an application-level POST protocol. No WDA client changes are required for ordinary requests.

Keep each validator with its verified JPEG in the requesting client's own cache. The harness binds that cache to one endpoint and keys it by all capture parameters. Identical final JPEG bytes can safely produce the same digest across parameter sets. The server holds only request-local data. It never uses a previous frame to answer a capture.

## Offline reproduction

```sh
python "$lab/mock.py"    # Pillow required for synthetic JPEG generation
```

This exercises independent readers, changing parameters, all three request modes, transition detection and a deliberately stale response. The mock checks 7 hits in 8 static requests and 4 detected transitions in 4 tap trials. Its timing values are fixtures.

## Parent's Calculator run

Start with Calculator visible, a clear display and verified numeric key coordinates in screen points. For a display-only comparison use a rectangle that contains the result and excludes the status bar and animated keys. Set X0, Y0, X1, Y1 and the two key positions for the active layout. Each --tap sends exactly one fastTap; the default run without taps only captures.

```sh
python "$lab/bench.py" --url "$WDA_URL" --static 60 --quality .8 --scale .5 \
  --rect "$X0" "$Y0" "$X1" "$Y1" --settle .4 \
  --tap "$KEY1_X" "$KEY1_Y" --tap "$KEY2_X" "$KEY2_Y" \
  --tap "$KEY1_X" "$KEY1_Y" --tap "$KEY2_X" "$KEY2_Y"
```

Repeat without --rect for full-screen hit rate, clearing the Calculator between runs. Static rounds rotate ordinary raw, instrumented baseline and conditional requests on one HTTP connection. The cold conditional request counts toward hit rate. The JSON reports p50/p95 round-trip time through the final response byte, server capture-plus-scale time and SHA-256-plus-tag generation time. The baseline skips hashing. The static comparison subtracts the ordinary raw p50 for end-to-end cost and the instrumented baseline p50 for capture cost. Bytes are JPEG and response body sizes, excluding HTTP headers, request JSON and transport overhead.

Each dynamic trial brackets one conditional capture with two independent unconditional captures after the tap settles. Agreement of both references on a changed frame requires a full, byte-identical conditional response. Differing references are inconclusive. Unchanged references count separately if the probe also agrees. A reference mismatch or zero detected transitions across a requested tap run exits nonzero. Inspect the inconclusive count before treating a run as evidence for every tap. Exact-byte changes can also reflect JPEG metadata or unrelated screen activity. These trials establish captured-byte transitions, not OCR recognition of the result.
