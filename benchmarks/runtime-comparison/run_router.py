#!/usr/bin/env python3
"""Run the agreed throwaway comparison without altering production services."""
import argparse
import datetime as dt
import hashlib
import io
import json
import pathlib
import shlex
import struct
import subprocess
import tarfile
import time

ROOT = pathlib.Path(__file__).resolve().parent
SSH = ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8']


def ssh(host, command, **kwargs):
    return subprocess.run([*SSH, host, command], check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--host', default='xkeen-router')
    parser.add_argument('--rounds', type=int, default=5)
    parser.add_argument('--cases', choices=('all', 'json'), default='all')
    parser.add_argument('--label', default='baseline')
    args = parser.parse_args()
    if args.rounds < 1 or args.rounds > 10:
        raise ValueError('rounds must be between 1 and 10')
    stamp = dt.datetime.now(dt.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    output = ROOT / 'results' / stamp
    output.mkdir(parents=True)
    auxv = ssh(args.host, 'cat /proc/self/auxv', capture_output=True, timeout=15).stdout
    hz = dict(struct.iter_unpack('<II', auxv)).get(17)
    if not hz:
        raise RuntimeError('AT_CLKTCK unavailable; refusing to guess CPU units')
    info = ssh(args.host, 'uname -a; cat /proc/cpuinfo; cat /proc/meminfo; cat /proc/loadavg; pidof xkeen-ui', capture_output=True, text=True, timeout=15).stdout
    (output / 'router-before.txt').write_text(info)
    artifacts = {name: {'sha256': hashlib.sha256((ROOT / 'bin' / name).read_bytes()).hexdigest(), 'bytes': (ROOT / 'bin' / name).stat().st_size} for name in ('server-go', 'server-rust', 'load')}
    metadata = {'started_at_utc': stamp, 'host': args.host, 'clock_ticks_per_second': hz, 'clock_source': '/proc/self/auxv AT_CLKTCK', 'rounds': args.rounds, 'cases': args.cases, 'label': args.label, 'duration_per_case_s': 8, 'artifacts': artifacts}
    (output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    remote = ssh(args.host, 'mktemp -d /tmp/xkeen-runtime-bench.XXXXXX', capture_output=True, text=True, timeout=15).stdout.strip()
    if not remote.startswith('/tmp/xkeen-runtime-bench.') or '/' in remote[len('/tmp/'):]:
        raise RuntimeError('Unexpected temporary directory')
    print(f'RESULTS {output}', flush=True)
    print(f'REMOTE {remote}; CLK_TCK={hz}', flush=True)
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode='w') as tar:
        for name in ('server-go', 'server-rust', 'load'):
            tar.add(ROOT / 'bin' / name, arcname=name)
        tar.add(ROOT / 'fixture.json', arcname='fixture.json')
    try:
        ssh(args.host, f'tar -xf - -C {shlex.quote(remote)}', input=archive.getvalue(), capture_output=True, timeout=60)
        cases = [('health', 1, 0), ('health', 4, 0), ('json', 1, 0), ('json', 4, 0), ('system', 1, 1)]
        if args.cases == 'json':
            cases = [case for case in cases if case[0] == 'json']
        with (output / 'raw.jsonl').open('w') as raw:
            for round_number in range(1, args.rounds + 1):
                order = ('go', 'rust') if round_number % 2 else ('rust', 'go')
                for language in order:
                    flags = '-listen' if language == 'go' else '--listen'
                    fixture_flag = '-fixture' if language == 'go' else '--fixture'
                    script = f'''set -eu
cd {shlex.quote(remote)}
{shlex.quote(remote)}/server-{language} {flags} 127.0.0.1:18081 {fixture_flag} fixture.json >{language}.log 2>&1 </dev/null &
pid=$!
echo "$pid" > active.pid
(sleep 90; kill "$pid" 2>/dev/null || true) </dev/null >/dev/null 2>&1 &
guardian=$!
cleanup() {{ kill "$guardian" 2>/dev/null || true; kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }}
trap cleanup EXIT HUP INT TERM
sleep 2
kill -0 "$pid"
curl --max-time 6 -fsS http://127.0.0.1:18081/health >/dev/null
'''
                    for endpoint, concurrency, rate in cases:
                        script += f'./load -url http://127.0.0.1:18081/{endpoint} -pid "$pid" -hz {hz} -concurrency {concurrency} -rate {rate} -duration 8s\n'
                    print(f'START round={round_number} language={language}', flush=True)
                    process = subprocess.Popen([*SSH, args.host, 'sh -s'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                    try:
                        process.stdin.write(script)
                        process.stdin.close()
                        index = -1
                        for index, line in enumerate(process.stdout):
                            result = json.loads(line)
                            endpoint, concurrency, rate = cases[index]
                            result.update({'round': round_number, 'language': language, 'endpoint': endpoint, 'measured_at_utc': dt.datetime.now(dt.timezone.utc).isoformat()})
                            if result['errors'] or result['successes'] < 1:
                                raise RuntimeError('Invalid measurement')
                            raw.write(json.dumps(result) + '\n'); raw.flush()
                            print(f'CASE round={round_number} language={language} endpoint={endpoint} c={concurrency} rps={result["rps"]:.1f} p95_ms={result["p95_ms"]:.3f} rss_kb={result["rss_after_kb"]} cpu_percent={result["server_cpu_percent"]:.2f}', flush=True)
                        stderr = process.stderr.read()
                        code = process.wait(timeout=15)
                        if code:
                            raise RuntimeError(f'Remote benchmark failed ({code}): {stderr}')
                        if index != len(cases)-1:
                            raise RuntimeError('Incomplete benchmark output')
                    finally:
                        if process.poll() is None:
                            process.terminate(); process.wait(timeout=10)
                    time.sleep(2)
        metadata['completed_at_utc'] = dt.datetime.now(dt.timezone.utc).isoformat()
        print('MEASUREMENTS COMPLETE', flush=True)
    finally:
        # Kill only our recorded PID if its executable still belongs to our directory.
        cleanup = f'''set -eu
if [ -f {shlex.quote(remote)}/active.pid ]; then
 pid=$(cat {shlex.quote(remote)}/active.pid)
 exe=$(readlink /proc/"$pid"/exe 2>/dev/null || true)
 case "$exe" in {shlex.quote(remote)}/*) kill "$pid" 2>/dev/null || true;; esac
fi
for exe in /proc/[0-9]*/exe; do
 target=$(readlink "$exe" 2>/dev/null || true)
 case "$target" in {shlex.quote(remote)}/*) echo "LEAKED_PROCESS $exe"; exit 1;; esac
done
rm -f {shlex.quote(remote)}/server-go {shlex.quote(remote)}/server-rust {shlex.quote(remote)}/load {shlex.quote(remote)}/fixture.json {shlex.quote(remote)}/go.log {shlex.quote(remote)}/rust.log {shlex.quote(remote)}/active.pid
rmdir {shlex.quote(remote)}
echo CLEANUP_OK
'''
        result = ssh(args.host, cleanup, capture_output=True, text=True, timeout=25)
        metadata['cleanup'] = result.stdout.strip()
        after = ssh(args.host, 'cat /proc/loadavg; head -5 /proc/meminfo; pidof xkeen-ui', capture_output=True, text=True, timeout=15).stdout
        (output / 'router-after.txt').write_text(after)
        (output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
        print(result.stdout.strip(), flush=True)


if __name__ == '__main__':
    main()
