#!/usr/bin/python3
"""Write an S3 object to stdout with the instance role: s3-cat.py REGION BUCKET KEY.
Ranged GETs in parallel, written in order, so one TCP stream's rate is not the limit."""
import sys
from concurrent.futures import ThreadPoolExecutor

import boto3

region, bucket, key = sys.argv[1:4]
s3 = boto3.client("s3", region_name=region)
size = s3.head_object(Bucket=bucket, Key=key)["ContentLength"]
PART = 64 << 20


def part(start):
    end = min(start + PART, size) - 1
    return s3.get_object(Bucket=bucket, Key=key, Range=f"bytes={start}-{end}")["Body"].read()


out = sys.stdout.buffer
WINDOW = 16  # parts in flight or waiting to be written: at most 1 GiB in memory
starts = list(range(0, size, PART))
with ThreadPoolExecutor(WINDOW) as pool:
    pending = [pool.submit(part, s) for s in starts[:WINDOW]]
    nxt = WINDOW
    while pending:
        out.write(pending.pop(0).result())
        if nxt < len(starts):
            pending.append(pool.submit(part, starts[nxt]))
            nxt += 1
out.flush()
