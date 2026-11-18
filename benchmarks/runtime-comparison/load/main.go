package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

type sample struct {
	CPU    uint64
	RSSKB  int64
	PeakKB int64
}

func readSample(pid int) sample {
	var result sample
	stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err == nil {
		fields := strings.Fields(string(stat)[strings.LastIndex(string(stat), ")")+1:])
		if len(fields) > 12 {
			u, _ := strconv.ParseUint(fields[11], 10, 64)
			s, _ := strconv.ParseUint(fields[12], 10, 64)
			result.CPU = u + s
		}
	}
	status, _ := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	for _, line := range strings.Split(string(status), "\n") {
		fields := strings.Fields(line)
		if len(fields) >= 2 {
			if fields[0] == "VmRSS:" {
				result.RSSKB, _ = strconv.ParseInt(fields[1], 10, 64)
			}
			if fields[0] == "VmHWM:" {
				result.PeakKB, _ = strconv.ParseInt(fields[1], 10, 64)
			}
		}
	}
	return result
}

func main() {
	url := flag.String("url", "http://127.0.0.1:18081/json", "Target URL")
	concurrency := flag.Int("concurrency", 1, "Concurrent clients")
	duration := flag.Duration("duration", 8*time.Second, "Measured interval")
	rate := flag.Float64("rate", 0, "Total requests per second, zero is maximum throughput")
	pid := flag.Int("pid", 0, "Server PID for Linux resource samples")
	hz := flag.Float64("hz", 0, "Verified CLK_TCK, zero omits CPU percentage")
	flag.Parse()
	if *concurrency < 1 || *concurrency > 32 || *duration <= 0 || *rate < 0 {
		panic("invalid benchmark parameters")
	}
	client := &http.Client{Timeout: 6 * time.Second, Transport: &http.Transport{Proxy: nil, MaxIdleConnsPerHost: 32}}
	request := func() (float64, bool) {
		start := time.Now()
		response, err := client.Get(*url)
		if err != nil {
			return float64(time.Since(start).Microseconds()) / 1000, false
		}
		body, err := io.ReadAll(io.LimitReader(response.Body, 1024*1024+1))
		response.Body.Close()
		return float64(time.Since(start).Microseconds()) / 1000, err == nil && response.StatusCode == 200 && len(body) <= 1024*1024 && json.Valid(body)
	}
	for i := 0; i < 5; i++ {
		if _, ok := request(); !ok {
			panic("warmup failed: expected HTTP 200 JSON")
		}
	}
	before := readSample(*pid)
	if *pid > 0 && before.RSSKB == 0 {
		panic("server resource sampling failed")
	}
	start := time.Now()
	deadline := start.Add(*duration)
	var lock sync.Mutex
	var latencies []float64
	failures := 0
	var wg sync.WaitGroup
	for worker := 0; worker < *concurrency; worker++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			var ticker *time.Ticker
			if *rate > 0 {
				ticker = time.NewTicker(time.Duration(float64(time.Second) * float64(*concurrency) / *rate))
				defer ticker.Stop()
			}
			for time.Now().Before(deadline) {
				if ticker != nil {
					<-ticker.C
					if !time.Now().Before(deadline) {
						break
					}
				}
				latency, ok := request()
				lock.Lock()
				if ok {
					latencies = append(latencies, latency)
				} else {
					failures++
				}
				lock.Unlock()
			}
		}()
	}
	wg.Wait()
	elapsed := time.Since(start).Seconds()
	after := readSample(*pid)
	if *pid > 0 && (after.RSSKB == 0 || after.CPU < before.CPU) {
		panic("server resource sampling failed")
	}
	sort.Float64s(latencies)
	percentile := func(q float64) float64 {
		if len(latencies) == 0 {
			return 0
		}
		return latencies[int(float64(len(latencies)-1)*q)]
	}
	result := map[string]any{"url": *url, "concurrency": *concurrency, "rate": *rate, "duration_s": elapsed, "successes": len(latencies), "errors": failures, "rps": float64(len(latencies)) / elapsed, "p50_ms": percentile(.5), "p95_ms": percentile(.95), "p99_ms": percentile(.99), "rss_before_kb": before.RSSKB, "rss_after_kb": after.RSSKB, "peak_rss_kb": after.PeakKB, "cpu_ticks": after.CPU - before.CPU, "client_on_router": true}
	if *hz > 0 {
		result["server_cpu_percent"] = float64(after.CPU-before.CPU) / *hz / elapsed * 100
	}
	_ = json.NewEncoder(os.Stdout).Encode(result)
	if failures > 0 || len(latencies) == 0 {
		os.Exit(1)
	}
}
