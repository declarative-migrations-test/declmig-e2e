package oresmiddleware

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	nextloggers "github.com/ores-otel/ores.otel.log/sdk/go"
)

func testConfig() Config {
	config := DefaultConfig("test")
	config.Settings.TLS.RequireHTTPS = false
	config.Settings.TLS.Mode = "disabled"
	config.Settings.RateLimit.Enabled = false
	return config
}

func TestProductionRejectsTestOnlyMiddleware(t *testing.T) {
	config := testConfig()
	config.Environment = Production
	config.Settings.FaultInjection.Enabled = true
	config.Settings.TestAuthBypass.Enabled = true
	issues := ValidateConfig(config)
	if len(issues) < 2 {
		t.Fatalf("expected production safety issues, got %#v", issues)
	}
}

func TestContextIsRequestScoped(t *testing.T) {
	value := RequestContext{RequestID: "r1", TraceID: "0123456789abcdef0123456789abcdef", Baggage: map[string]string{}}
	_, err := RunWithContext(context.Background(), value, func(ctx context.Context) (struct{}, error) {
		current, ok := CurrentContext(ctx)
		if !ok || current.RequestID != "r1" {
			t.Fatal("missing request context")
		}
		return struct{}{}, nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

func TestStackAddsRequestAndSecurityHeaders(t *testing.T) {
	stack, err := New(testConfig(), Dependencies{})
	if err != nil {
		t.Fatal(err)
	}
	handler := stack.Wrap(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if _, ok := CurrentContext(request.Context()); !ok {
			t.Fatal("context not installed")
		}
		writer.Header().Set("Content-Type", "application/json")
		_, _ = writer.Write([]byte(`{"ok":true}`))
	}))
	request := httptest.NewRequest(http.MethodGet, "http://example.test/v1", nil)
	request.Header.Set("Accept", "application/json")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("status %d: %s", response.Code, response.Body.String())
	}
	if response.Header().Get("x-request-id") == "" {
		t.Fatal("missing request ID response header")
	}
	if response.Header().Get("traceparent") != "" {
		t.Fatal("middleware must not synthesize a response traceparent without a server span")
	}
	if response.Header().Get("x-content-type-options") != "nosniff" {
		t.Fatal("missing security headers")
	}
}

type denyRateLimiter struct{}

func (denyRateLimiter) Allow(context.Context, string, int, float64) (bool, error) {
	return false, nil
}

func TestRateLimitDenialExposesBackpressureMetadata(t *testing.T) {
	config := testConfig()
	config.Settings.RateLimit.Enabled = true
	stack, err := New(config, Dependencies{RateLimiter: denyRateLimiter{}})
	if err != nil {
		t.Fatal(err)
	}
	handler := stack.Wrap(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		t.Fatal("handler must not run")
	}))
	request := httptest.NewRequest(http.MethodGet, "http://example.test/v1", nil)
	request.RemoteAddr = "203.0.113.9:4242"
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)

	if response.Code != http.StatusTooManyRequests {
		t.Fatalf("status %d: %s", response.Code, response.Body.String())
	}
	expected := map[string]string{
		"Retry-After":                "1",
		"RateLimit-Policy":           "\"ip-default\";q=5;w=1",
		"RateLimit":                  "\"ip-default\";r=0;t=1",
		"RateLimit-Limit":            "5",
		"RateLimit-Remaining":        "0",
		"RateLimit-Reset":            "1",
		"X-Ores-Rate-Limit-Policy":   "ip-default",
		"X-Ores-Rate-Limit-Layer":    "application",
		"X-Ores-Rate-Limit-Decision": "denied",
	}
	for name, want := range expected {
		if got := response.Header().Get(name); got != want {
			t.Fatalf("%s=%q, want %q", name, got, want)
		}
	}
}

func TestStrictForwardedClientIdentityRejectsUntrustedPeer(t *testing.T) {
	config := testConfig()
	config.Settings.TLS.StrictForwardedHeaders = true
	stack, err := New(config, Dependencies{})
	if err != nil {
		t.Fatal(err)
	}
	handler := stack.Wrap(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		t.Fatal("handler must not run")
	}))
	request := httptest.NewRequest(http.MethodGet, "http://example.test/v1", nil)
	request.RemoteAddr = "198.51.100.10:4242"
	request.Header.Set("X-Forwarded-For", "203.0.113.9")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest {
		t.Fatalf("status %d: %s", response.Code, response.Body.String())
	}
}

func TestTrustedProxyClientIPPrefersCanonicalForwardedIdentity(t *testing.T) {
	request := httptest.NewRequest(http.MethodGet, "http://example.test/v1", nil)
	request.RemoteAddr = "127.0.0.1:4242"
	request.Header.Set("CF-Connecting-IP", "203.0.113.055")
	request.Header.Set("X-Forwarded-For", "203.0.113.55, 10.0.0.4")
	if got := clientIP(request, true); got != "203.0.113.55" {
		t.Fatalf("clientIP=%q", got)
	}
}

func TestMemoryTokenBucketBoundsSourceIPCardinality(t *testing.T) {
	limiter := NewMemoryTokenBucket(func() time.Time { return time.Unix(0, 0) })
	for index := 0; index <= defaultLocalRateLimitMaxEntries; index++ {
		key := fmt.Sprintf("ip-%d", index)
		allowed, err := limiter.Allow(context.Background(), key, 1, 0.000001)
		if err != nil || !allowed {
			t.Fatalf("key %q allowed=%v err=%v", key, allowed, err)
		}
	}
	if got := len(limiter.buckets); got != defaultLocalRateLimitMaxEntries {
		t.Fatalf("bucket count=%d", got)
	}
	if _, exists := limiter.buckets["ip-0"]; exists {
		t.Fatal("oldest bucket was not evicted")
	}
	if limiter.order.Len() != defaultLocalRateLimitMaxEntries {
		t.Fatalf("order length=%d", limiter.order.Len())
	}
}

func TestDescriptorExportsStandardOperations(t *testing.T) {
	value := Descriptor()
	if len(value.OperationSymbols) != 7 {
		t.Fatalf("operations=%d", len(value.OperationSymbols))
	}
	if len(value.Capabilities) != len(Capabilities) {
		t.Fatal("capability mismatch")
	}
}

type lifecycleReport struct {
	failure      OperationFailure
	requestID    string
	userID       string
	logRequestID any
	logUserID    any
}

type panicFinishedTelemetry struct{}

func (panicFinishedTelemetry) Started(context.Context, RequestContext, *http.Request) {}
func (panicFinishedTelemetry) Finished(context.Context, RequestContext, *http.Request, int, time.Duration) {
	panic("private telemetry detail")
}

func TestAuthenticationPanicIsContainedInsideBaseRequestAndLogContext(t *testing.T) {
	reports := make(chan lifecycleReport, 1)
	stack, err := New(testConfig(), Dependencies{
		AuthVerifier: authVerifierFunc(func(ctx context.Context, _ *http.Request, value RequestContext) (AuthDecision, error) {
			current, ok := CurrentContext(ctx)
			if !ok || current.RequestID != value.RequestID {
				t.Fatalf("missing base request context: %#v", current)
			}
			logContext, ok := nextloggers.LogContextFrom(ctx)
			if !ok || logContext.Fields["request.id"] != value.RequestID {
				t.Fatalf("missing base ores-otel context: %#v", logContext)
			}
			panic("private authentication detail")
		}),
		OperationFailureReporter: func(ctx context.Context, failure OperationFailure) {
			current, _ := CurrentContext(ctx)
			logContext, _ := nextloggers.LogContextFrom(ctx)
			reports <- lifecycleReport{
				failure:      failure,
				requestID:    current.RequestID,
				userID:       current.UserID,
				logRequestID: logContext.Fields["request.id"],
				logUserID:    logContext.Fields["user.id"],
			}
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	handler := stack.Wrap(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		t.Fatal("handler must not run")
	}))
	request := httptest.NewRequest(http.MethodGet, "http://example.test/profile", nil)
	request.Header.Set("Accept", "application/json")
	request.Header.Set("X-Request-ID", "auth-panic")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)

	if response.Code != http.StatusInternalServerError {
		t.Fatalf("status %d: %s", response.Code, response.Body.String())
	}
	if response.Header().Get("X-Request-ID") != "auth-panic" {
		t.Fatalf("missing request correlation: %#v", response.Header())
	}
	if strings.Contains(response.Body.String(), "private authentication detail") {
		t.Fatal("panic detail leaked into response")
	}
	report := <-reports
	if report.failure.Kind != OperationFailurePanic || report.failure.RequestID != "auth-panic" {
		t.Fatalf("unexpected failure: %#v", report.failure)
	}
	if report.requestID != "auth-panic" || report.logRequestID != "auth-panic" {
		t.Fatalf("reporter lost base context: %#v", report)
	}
}

func TestFinalizationPanicRetainsAuthenticatedActorContext(t *testing.T) {
	reports := make(chan lifecycleReport, 1)
	stack, err := New(testConfig(), Dependencies{
		AuthVerifier: authVerifierFunc(func(context.Context, *http.Request, RequestContext) (AuthDecision, error) {
			return AuthDecision{UserID: "user-42", TenantID: "tenant-7", Claims: map[string]string{"otel.plan": "pro", "private": "drop"}}, nil
		}),
		Telemetry: panicFinishedTelemetry{},
		OperationFailureReporter: func(ctx context.Context, failure OperationFailure) {
			current, _ := CurrentContext(ctx)
			logContext, _ := nextloggers.LogContextFrom(ctx)
			reports <- lifecycleReport{
				failure:      failure,
				requestID:    current.RequestID,
				userID:       current.UserID,
				logRequestID: logContext.Fields["request.id"],
				logUserID:    logContext.Fields["user.id"],
			}
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	handler := stack.Wrap(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		current, ok := CurrentContext(request.Context())
		if !ok || current.UserID != "user-42" || current.TenantID != "tenant-7" {
			t.Fatalf("missing authenticated request context: %#v", current)
		}
		logContext, ok := nextloggers.LogContextFrom(request.Context())
		if !ok || logContext.Fields["user.id"] != "user-42" || logContext.Fields["tenant.id"] != "tenant-7" {
			t.Fatalf("missing authenticated ores-otel context: %#v", logContext)
		}
		writer.Header().Set("Content-Type", "application/json")
		_, _ = writer.Write([]byte(`{"ok":true}`))
	}))
	request := httptest.NewRequest(http.MethodGet, "http://example.test/profile", nil)
	request.Header.Set("Accept", "application/json")
	request.Header.Set("X-Request-ID", "finish-panic")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)

	if response.Code != http.StatusInternalServerError {
		t.Fatalf("status %d: %s", response.Code, response.Body.String())
	}
	if strings.Contains(response.Body.String(), "private telemetry detail") {
		t.Fatal("telemetry panic detail leaked into response")
	}
	report := <-reports
	if report.userID != "user-42" || report.logUserID != "user-42" {
		t.Fatalf("reporter lost authenticated actor context: %#v", report)
	}
}

func TestBufferedResponseRejectsWritesAfterDoneSignal(t *testing.T) {
	done := make(chan struct{})
	capture := newBufferedResponseWithDone(done)
	close(done)

	if _, err := capture.Write([]byte("late")); !errors.Is(err, http.ErrHandlerTimeout) {
		t.Fatalf("late write error = %v", err)
	}
	capture.WriteHeader(http.StatusCreated)
	status, _, body := capture.snapshot()
	if status != http.StatusOK {
		t.Fatalf("late WriteHeader changed status to %d", status)
	}
	if len(body) != 0 {
		t.Fatalf("late write mutated body: %q", body)
	}
}

func TestDeadlineSealsHandlerBufferAgainstLateWrites(t *testing.T) {
	config := testConfig()
	config.Settings.TimeoutMS = 5
	reports := make(chan OperationFailure, 1)
	stack, err := New(config, Dependencies{
		OperationFailureReporter: func(_ context.Context, failure OperationFailure) {
			reports <- failure
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	lateWrite := make(chan error, 1)
	handler := stack.Wrap(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		<-request.Context().Done()
		_, writeErr := writer.Write([]byte("late response must be rejected"))
		lateWrite <- writeErr
	}))
	request := httptest.NewRequest(http.MethodGet, "http://example.test/slow", nil)
	request.Header.Set("Accept", "application/json")
	request.Header.Set("X-Request-ID", "deadline-1")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)

	if response.Code != http.StatusGatewayTimeout {
		t.Fatalf("status %d: %s", response.Code, response.Body.String())
	}
	failure := <-reports
	if failure.Kind != OperationFailureDeadlineExceeded || failure.RequestID != "deadline-1" {
		t.Fatalf("unexpected timeout failure: %#v", failure)
	}
	select {
	case writeErr := <-lateWrite:
		if !errors.Is(writeErr, http.ErrHandlerTimeout) {
			t.Fatalf("late write error = %v", writeErr)
		}
	case <-time.After(250 * time.Millisecond):
		t.Fatal("late handler did not finish")
	}
	if strings.Contains(response.Body.String(), "late response") {
		t.Fatal("late handler write mutated completed response")
	}
}
