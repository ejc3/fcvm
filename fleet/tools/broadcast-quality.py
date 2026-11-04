#!/usr/bin/env python3
"""How the broadcast looks to a viewer (docs/fleet/ARCHITECTURE.md, "The composite").

  broadcast-quality.py compose RUNDIR COLS OUT.yuv
      the aggregator's composite of a `fleet-renderer quality --encode SH` run's q=std tile
      streams: each decoded to YUV 4:4:4, its overlap cropped, placed on the image's grid
  broadcast-quality.py encode SRC.yuv W H OUT.flv KBPS CRF
      the aggregator's libx264 command line (src/fleet/aggregate/codec.rs)
  broadcast-quality.py score ENC.flv REF.ppm [TRANSCODE_KBPS]
      the decoded broadcast's last frame, scaled bilinearly to the reference's size as a page
      scales it, against REF.ppm (`quality`'s held reference); with TRANSCODE_KBPS, after a
      libx264 transcode at that rate first, standing in for an IVS rendition

Needs ffmpeg, numpy, scipy, Pillow and flip-evaluator (pip).
"""
import json, os, re, subprocess, sys, tempfile

import numpy as np
from PIL import Image
from scipy.ndimage import gaussian_filter


def frames444(path):
    wh = subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_entries', 'stream=width,height',
                         '-of', 'csv=p=0', path], check=True, capture_output=True, text=True).stdout.strip().split(',')
    w, h = int(wh[0]), int(wh[1])
    raw = subprocess.run(['ffmpeg', '-v', 'error', '-i', path, '-f', 'rawvideo', '-pix_fmt', 'yuv444p', '-'],
                         check=True, capture_output=True).stdout
    return np.frombuffer(raw, np.uint8).reshape(-1, 3, h, w)


def rgb(f):  # BT.709 limited range, as the streams are tagged
    y = (f[0].astype(np.float64) - 16) * 255 / 219
    cb = (f[1].astype(np.float64) - 128) * 255 / 224
    cr = (f[2].astype(np.float64) - 128) * 255 / 224
    return np.clip(np.round(np.stack([y + 1.5748 * cr, y - 0.1873 * cb - 0.4681 * cr, y + 1.8556 * cb], -1)), 0, 255)


def compose(run, cols, out):
    err = open(os.path.join(run, 'stderr.txt')).read()
    tiles = {}
    for m in re.finditer(r'tile (\d+) std: (\d+)x(\d+) crop l(\d+) t(\d+) r(\d+) b(\d+)', err):
        ti, ew, eh, l, t, r, b = map(int, m.groups())
        tiles[ti] = frames444(os.path.join(run, f'tile{ti}-std.h264'))[:, :, t:eh - b, l:ew - r]
    rows = len(tiles) // cols
    ws = [tiles[c].shape[3] for c in range(cols)]
    hs = [tiles[r * cols].shape[2] for r in range(rows)]
    n = min(v.shape[0] for v in tiles.values())
    comp = np.zeros((n, 3, sum(hs), sum(ws)), np.uint8)
    for ti, v in tiles.items():
        x0, y0 = sum(ws[:ti % cols]), sum(hs[:ti // cols])
        comp[:, :, y0:y0 + v.shape[2], x0:x0 + v.shape[3]] = v[:n]
    comp.tofile(out)
    print(json.dumps({'width': sum(ws), 'height': sum(hs), 'frames': n}))


def encode(src, w, h, out, kbps, crf):
    subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-f', 'rawvideo', '-pix_fmt', 'yuv444p',
                    '-s', f'{w}x{h}', '-framerate', '30', '-i', src, '-fps_mode', 'passthrough', '-c:v', 'libx264',
                    '-preset', 'veryfast', '-tune', 'zerolatency', '-profile:v', 'high', '-pix_fmt', 'yuv420p',
                    '-bf', '0', '-g', '60', '-keyint_min', '60', '-sc_threshold', '0', '-crf', str(crf),
                    '-maxrate', f'{kbps}k', '-bufsize', f'{int(kbps) // 2}k', '-x264-params', 'repeat-headers=1',
                    '-color_range', 'tv', '-colorspace', 'bt709', '-color_primaries', 'bt709', '-color_trc', 'bt709',
                    '-f', 'flv', '-y', out], check=True)


def score(enc, ref, transcode_kbps=None):
    import flip_evaluator
    with tempfile.TemporaryDirectory() as td:
        if transcode_kbps:
            t = os.path.join(td, 't.flv')
            k = f'{transcode_kbps}k'
            subprocess.run(['ffmpeg', '-v', 'error', '-i', enc, '-c:v', 'libx264', '-preset', 'veryfast', '-b:v', k,
                            '-maxrate', k, '-bufsize', k, '-bf', '0', '-g', '60', '-an', '-f', 'flv', '-y', t], check=True)
            enc = t
        last = rgb(frames444(enc)[-1])
    R = np.asarray(Image.open(ref).convert('RGB')).astype(np.float64)
    up = np.stack([np.asarray(Image.fromarray(last[..., c].astype(np.float32), mode='F')
                              .resize((R.shape[1], R.shape[0]), Image.BILINEAR)) for c in range(3)], -1)
    up = np.clip(up, 0, 255)
    L = lambda a: 0.2126 * a[..., 0] + 0.7152 * a[..., 1] + 0.0722 * a[..., 2]
    x, y = L(up), L(R)
    mx, my = gaussian_filter(x, 1.5), gaussian_filter(y, 1.5)
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    vx, vy = gaussian_filter(x * x, 1.5) - mx * mx, gaussian_filter(y * y, 1.5) - my * my
    cxy = gaussian_filter(x * y, 1.5) - mx * my
    ssim = np.mean(((2 * mx * my + c1) * (2 * cxy + c2)) / ((mx * mx + my * my + c1) * (vx + vy + c2)))
    print(json.dumps({'psnr': round(float(10 * np.log10(255 ** 2 / np.mean((up - R) ** 2))), 2),
                      'ssim': round(float(ssim), 4),
                      'flip': round(float(flip_evaluator.evaluate(R.astype(np.float32) / 255,
                                                                  up.astype(np.float32) / 255, 'LDR')[1]), 4)}))


if __name__ == '__main__':
    a = sys.argv
    if a[1] == 'compose':
        compose(a[2], int(a[3]), a[4])
    elif a[1] == 'encode':
        encode(*a[2:8])
    elif a[1] == 'score':
        score(a[2], a[3], a[4] if len(a) > 4 else None)
    else:
        sys.exit(__doc__)
