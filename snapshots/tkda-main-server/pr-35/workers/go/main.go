package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

const elementKey = "element-6066-11e4-a52e-4f735466cecf"
const maxWebDriverResponseBytes = 4 * 1024 * 1024

type Command struct {
	Type          string          `json:"type"`
	RunID         string          `json:"run_id,omitempty"`
	LeaseEpoch    uint64          `json:"lease_epoch,omitempty"`
	RequestID     string          `json:"request_id,omitempty"`
	Method        string          `json:"method,omitempty"`
	Path          string          `json:"path,omitempty"`
	Reason        string          `json:"reason,omitempty"`
	BrowserEngine string          `json:"browser_engine,omitempty"`
	Body          json.RawMessage `json:"body,omitempty"`
}

type webdriverEnvelope struct {
	Value any `json:"value"`
}

type workerState struct {
	client        *http.Client
	upstreamURL   string
	browserName   string
	browserEngine string
	sessionID     string
	leaseEpoch    uint64
}

func emit(value any) {
	encoded, err := json.Marshal(value)
	if err != nil {
		return
	}
	fmt.Println(string(encoded))
}

func emitEvent(leaseEpoch uint64, value map[string]any) {
	if leaseEpoch == 0 {
		return
	}
	value["lease_epoch"] = leaseEpoch
	emit(value)
}

func getenv(name string, fallback string) string {
	if value := strings.TrimSpace(os.Getenv(name)); value != "" {
		return value
	}
	return fallback
}

func validateSeleniumUpstreamURL(raw string) (string, error) {
	parsed, err := url.Parse(raw)
	if err != nil {
		return "", fmt.Errorf("TKDA_SELENIUM_UPSTREAM_URL must be a valid loopback HTTP URL")
	}
	host := parsed.Hostname()
	if parsed.Scheme != "http" ||
		(host != "127.0.0.1" && host != "::1") ||
		parsed.Port() == "" ||
		parsed.User != nil ||
		(parsed.Path != "" && parsed.Path != "/") ||
		parsed.RawQuery != "" ||
		parsed.Fragment != "" {
		return "", fmt.Errorf("TKDA_SELENIUM_UPSTREAM_URL must be credential-free root HTTP on literal loopback with an explicit port")
	}
	return strings.TrimRight(parsed.String(), "/"), nil
}

func (state *workerState) webdriver(method string, path string, body any) (any, error) {
	var reader io.Reader
	if body != nil {
		encoded, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		reader = bytes.NewReader(encoded)
	}

	request, err := http.NewRequest(method, state.upstreamURL+path, reader)
	if err != nil {
		return nil, err
	}
	request.Header.Set("accept", "application/json")
	if body != nil {
		request.Header.Set("content-type", "application/json")
	}

	response, err := state.client.Do(request)
	if err != nil {
		return nil, fmt.Errorf("WebDriver upstream unavailable at %s: %w", state.upstreamURL, err)
	}
	defer response.Body.Close()

	payload, err := io.ReadAll(io.LimitReader(response.Body, maxWebDriverResponseBytes+1))
	if err != nil {
		return nil, err
	}
	if len(payload) > maxWebDriverResponseBytes {
		return nil, fmt.Errorf("WebDriver upstream response exceeded %d bytes", maxWebDriverResponseBytes)
	}
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		detail := string(payload)
		if len(detail) > 20_000 {
			detail = detail[:20_000]
		}
		return nil, fmt.Errorf("WebDriver upstream returned HTTP %d: %s", response.StatusCode, detail)
	}

	var envelope webdriverEnvelope
	if len(payload) > 0 {
		if err := json.Unmarshal(payload, &envelope); err != nil {
			return nil, fmt.Errorf("invalid WebDriver upstream JSON: %w", err)
		}
	}

	if value, ok := envelope.Value.(map[string]any); ok {
		if code, ok := value["error"].(string); ok && code != "" {
			return nil, fmt.Errorf("WebDriver upstream error %s: %v", code, value["message"])
		}
	}
	return envelope.Value, nil
}

func (state *workerState) ensureSession() (string, error) {
	if state.sessionID != "" {
		return state.sessionID, nil
	}

	value, err := state.webdriver("POST", "/session", map[string]any{
		"capabilities": map[string]any{
			"alwaysMatch": map[string]any{"browserName": state.browserName},
			"firstMatch":  []any{map[string]any{}},
		},
	})
	if err != nil {
		return "", err
	}

	object, ok := value.(map[string]any)
	if !ok {
		return "", fmt.Errorf("WebDriver upstream session response did not contain an object")
	}
	candidate, _ := object["sessionId"].(string)
	if candidate == "" {
		return "", fmt.Errorf("WebDriver upstream session response omitted sessionId")
	}
	state.sessionID = candidate
	return candidate, nil
}

func (state *workerState) findElement(selector string) (string, error) {
	sessionID, err := state.ensureSession()
	if err != nil {
		return "", err
	}
	value, err := state.webdriver("POST", "/session/"+sessionID+"/element", map[string]any{
		"using": "css selector",
		"value": selector,
	})
	if err != nil {
		return "", err
	}
	object, ok := value.(map[string]any)
	if !ok {
		return "", fmt.Errorf("WebDriver element response did not contain an object")
	}
	elementID, _ := object[elementKey].(string)
	if elementID == "" {
		return "", fmt.Errorf("WebDriver element response omitted the W3C element id")
	}
	return elementID, nil
}

func (state *workerState) execute(action map[string]any) (any, error) {
	if state.browserEngine != "selenium" {
		return nil, fmt.Errorf("unsupported: Go worker currently supports browser_engine=selenium, not %s", state.browserEngine)
	}

	operation, _ := action["op"].(string)
	sessionID, err := state.ensureSession()
	if err != nil {
		return nil, err
	}

	switch operation {
	case "navigate":
		url, _ := action["url"].(string)
		if url == "" {
			return nil, fmt.Errorf("unsupported: navigate requires url")
		}
		if _, err := state.webdriver("POST", "/session/"+sessionID+"/url", map[string]any{"url": url}); err != nil {
			return nil, err
		}
		current, err := state.webdriver("GET", "/session/"+sessionID+"/url", nil)
		return map[string]any{"url": current}, err
	case "url", "current_url":
		current, err := state.webdriver("GET", "/session/"+sessionID+"/url", nil)
		return map[string]any{"url": current}, err
	case "title":
		title, err := state.webdriver("GET", "/session/"+sessionID+"/title", nil)
		return map[string]any{"title": title}, err
	case "click", "fill", "text":
		selector, _ := action["selector"].(string)
		if selector == "" {
			selector = "body"
		}
		elementID, err := state.findElement(selector)
		if err != nil {
			return nil, err
		}
		elementPath := "/session/" + sessionID + "/element/" + elementID
		if operation == "click" {
			_, err := state.webdriver("POST", elementPath+"/click", map[string]any{})
			return map[string]any{"ok": err == nil}, err
		}
		if operation == "fill" {
			value := fmt.Sprint(action["value"])
			characters := make([]string, 0, len([]rune(value)))
			for _, character := range []rune(value) {
				characters = append(characters, string(character))
			}
			_, err := state.webdriver("POST", elementPath+"/value", map[string]any{"text": value, "value": characters})
			return map[string]any{"ok": err == nil}, err
		}
		text, err := state.webdriver("GET", elementPath+"/text", nil)
		return map[string]any{"text": fmt.Sprint(text)}, err
	case "screenshot":
		image, err := state.webdriver("GET", "/session/"+sessionID+"/screenshot", nil)
		return map[string]any{"screenshot_base64": image, "omitted": false}, err
	case "sleep":
		milliseconds := 250
		if raw, ok := action["milliseconds"].(float64); ok {
			milliseconds = int(raw)
		}
		if milliseconds < 0 {
			milliseconds = 0
		}
		if milliseconds > 30_000 {
			milliseconds = 30_000
		}
		time.Sleep(time.Duration(milliseconds) * time.Millisecond)
		return map[string]any{"slept_ms": milliseconds}, nil
	default:
		return nil, fmt.Errorf("unsupported: unsupported Go Selenium action %q", operation)
	}
}

func (state *workerState) closeSession() {
	if state.sessionID == "" {
		return
	}
	sessionID := state.sessionID
	state.sessionID = ""
	if _, err := state.webdriver("DELETE", "/session/"+sessionID, nil); err != nil {
		if state.leaseEpoch > 0 {
			emitEvent(state.leaseEpoch, map[string]any{"type": "log", "level": "warn", "message": err.Error()})
		} else {
			fmt.Fprintf(os.Stderr, "failed to close WebDriver session before start: %v\n", err)
		}
	}
}

func main() {
	upstreamURL, err := validateSeleniumUpstreamURL(
		getenv("TKDA_SELENIUM_UPSTREAM_URL", "http://127.0.0.1:9515"),
	)
	if err != nil {
		fmt.Fprintf(os.Stderr, "invalid Selenium worker configuration: %v\n", err)
		return
	}
	state := &workerState{
		client: &http.Client{
			Timeout: 30 * time.Second,
			CheckRedirect: func(_ *http.Request, _ []*http.Request) error {
				return http.ErrUseLastResponse
			},
		},
		upstreamURL:   upstreamURL,
		browserName:   getenv("TKDA_SELENIUM_BROWSER", "chrome"),
		browserEngine: "selenium",
	}
	defer state.closeSession()

	fmt.Fprintf(os.Stderr, "go adapter boot pid=%d upstream=%s\n", os.Getpid(), state.upstreamURL)

	scanner := bufio.NewScanner(os.Stdin)
	buffer := make([]byte, 64*1024)
	scanner.Buffer(buffer, 1024*1024)

	for scanner.Scan() {
		var command Command
		if err := json.Unmarshal(scanner.Bytes(), &command); err != nil {
			if state.leaseEpoch > 0 {
				emitEvent(state.leaseEpoch, map[string]any{"type": "failed", "retryable": false, "error": err.Error()})
			} else {
				fmt.Fprintf(os.Stderr, "invalid worker command before start: %v\n", err)
			}
			continue
		}

		if command.Type != "start" && state.leaseEpoch == 0 {
			fmt.Fprintf(os.Stderr, "refusing %s command before a positive lease_epoch is established\n", command.Type)
			continue
		}

		switch command.Type {
		case "start":
			if command.LeaseEpoch == 0 {
				fmt.Fprintln(os.Stderr, "start command requires positive lease_epoch")
				return
			}
			state.leaseEpoch = command.LeaseEpoch
			if command.BrowserEngine != "" {
				state.browserEngine = command.BrowserEngine
			}
			emitEvent(state.leaseEpoch, map[string]any{"type": "ready", "transport": "stdio"})
			emitEvent(state.leaseEpoch, map[string]any{
				"type":    "log",
				"level":   "info",
				"message": fmt.Sprintf("go worker ready for run %s engine=%s", command.RunID, state.browserEngine),
			})
		case "driver":
			var action map[string]any
			if err := json.Unmarshal(command.Body, &action); err != nil {
				emitEvent(state.leaseEpoch, map[string]any{
					"type":       "driver_response",
					"request_id": command.RequestID,
					"status":     400,
					"body":       map[string]any{"error": err.Error()},
				})
				continue
			}
			result, err := state.execute(action)
			if err != nil {
				status := 502
				if strings.HasPrefix(err.Error(), "unsupported:") {
					status = 501
				}
				emitEvent(state.leaseEpoch, map[string]any{
					"type":       "driver_response",
					"request_id": command.RequestID,
					"status":     status,
					"body":       map[string]any{"error": strings.TrimPrefix(err.Error(), "unsupported: ")},
				})
				continue
			}
			emitEvent(state.leaseEpoch, map[string]any{
				"type":       "driver_response",
				"request_id": command.RequestID,
				"status":     200,
				"body":       result,
			})
		case "replan":
			emitEvent(state.leaseEpoch, map[string]any{"type": "needs_replan", "reason": command.Reason})
		case "cancel":
			state.closeSession()
			emitEvent(state.leaseEpoch, map[string]any{"type": "log", "level": "info", "message": "cancelled: " + command.Reason})
			return
		default:
			emitEvent(state.leaseEpoch, map[string]any{
				"type":      "failed",
				"retryable": false,
				"error":     "unknown command type: " + command.Type,
			})
		}
	}

	if err := scanner.Err(); err != nil {
		if state.leaseEpoch > 0 {
			emitEvent(state.leaseEpoch, map[string]any{"type": "failed", "retryable": true, "error": err.Error()})
		} else {
			fmt.Fprintf(os.Stderr, "worker stdin failed before start: %v\n", err)
		}
		os.Exit(1)
	}
}
