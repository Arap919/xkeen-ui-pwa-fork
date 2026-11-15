package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"time"
)

func main() {
	address := flag.String("listen", "127.0.0.1:18081", "Listen address")
	fixturePath := flag.String("fixture", "fixture.json", "Shared JSON fixture")
	flag.Parse()
	fixture, err := os.ReadFile(*fixturePath)
	if err != nil {
		panic(err)
	}
	var data any
	decoder := json.NewDecoder(bytes.NewReader(fixture))
	decoder.UseNumber()
	if err := decoder.Decode(&data); err != nil {
		panic(err)
	}
	var config struct {
		XKeen struct {
			Token string `json:"rci_token"`
		} `json:"xkeen"`
	}
	if content, err := os.ReadFile("/opt/etc/xkeen/xkeen.json"); err == nil {
		_ = json.Unmarshal(content, &config)
	}
	client := &http.Client{Timeout: 5 * time.Second, Transport: &http.Transport{Proxy: nil, MaxIdleConnsPerHost: 32, IdleConnTimeout: 30 * time.Second}}
	mux := http.NewServeMux()
	mux.HandleFunc("GET /health", func(w http.ResponseWriter, r *http.Request) { writeJSON(w, map[string]any{"success": true}) })
	mux.HandleFunc("GET /json", func(w http.ResponseWriter, r *http.Request) { writeJSON(w, data) })
	mux.HandleFunc("GET /system", func(w http.ResponseWriter, r *http.Request) {
		url := os.Getenv("BENCH_RCI_URL")
		if url == "" {
			url = "http://127.0.0.1:79/rci/show/system"
		}
		req, err := http.NewRequestWithContext(r.Context(), "GET", url, nil)
		if err != nil {
			http.Error(w, "invalid upstream", 502)
			return
		}
		if config.XKeen.Token != "" {
			req.Header.Set("X-Ndma-Tkn", config.XKeen.Token)
		}
		res, err := client.Do(req)
		if err != nil {
			http.Error(w, "upstream unavailable", 502)
			return
		}
		defer res.Body.Close()
		if res.StatusCode != 200 {
			http.Error(w, "upstream status", 502)
			return
		}
		var value any
		decoder := json.NewDecoder(io.LimitReader(res.Body, 1024*1024))
		decoder.UseNumber()
		if err := decoder.Decode(&value); err != nil {
			http.Error(w, "invalid upstream JSON", 502)
			return
		}
		writeJSON(w, value)
	})
	server := &http.Server{Addr: *address, Handler: mux, ReadHeaderTimeout: 5 * time.Second, IdleTimeout: 30 * time.Second}
	fmt.Fprintln(os.Stderr, "benchmark server", *address)
	if err := server.ListenAndServe(); err != nil {
		panic(err)
	}
}

func writeJSON(w http.ResponseWriter, value any) {
	content, err := json.Marshal(value)
	if err != nil {
		http.Error(w, "JSON error", 500)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	_, _ = w.Write(content)
}
