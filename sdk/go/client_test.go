// F036: contract tests for the generated Go client over httptest —
// bearer injection, the possession path, idempotency, cursor queries,
// the SSE drain, and RFC 9457 classification.
package fishpond

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func testClient(t *testing.T, handler http.HandlerFunc) *Client {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	return NewClient(server.URL, func() string { return "key-1" })
}

func requireNoError(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
}

func requireError(t *testing.T, err error) *Error {
	t.Helper()
	if err == nil {
		t.Fatal("expected an error")
	}
	apiError, ok := err.(*Error)
	if !ok {
		t.Fatalf("expected *Error, got %T", err)
	}
	return apiError
}

func problemBody(slug string, status int) []byte {
	payload, _ := json.Marshal(map[string]any{
		"type":   problemBase + slug,
		"title":  "title text",
		"status": status,
		"detail": "detail text",
	})
	return payload
}

func TestBearerInjected(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Authorization"); got != "Bearer key-1" {
			t.Errorf("authorization = %q", got)
		}
		if r.URL.Path != "/accounts/acc-1" {
			t.Errorf("path = %q", r.URL.Path)
		}
		_, _ = w.Write([]byte(`{"id":"acc-1","platform":"qq","state":"ONLINE","updated_at":1}`))
	})
	account, err := client.GetAccount(context.Background(), "acc-1")
	requireNoError(t, err)
	if account.Id != "acc-1" || account.State != "ONLINE" {
		t.Errorf("account = %+v", account)
	}
}

func TestPathEscaped(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if escaped := r.URL.EscapedPath(); escaped != "/accounts/a%20b%2Fc" {
			t.Errorf("escaped path = %q", escaped)
		}
		_, _ = w.Write([]byte(`{"id":"a b/c"}`))
	})
	_, err := client.GetAccount(context.Background(), "a b/c")
	requireNoError(t, err)
}

func TestHealthIsAnonymous(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if auth := r.Header.Get("Authorization"); auth != "" {
			t.Errorf("health sent authorization %q", auth)
		}
		_, _ = w.Write([]byte(`{"status":"ok","service":"fishpond-rest","version":"1"}`))
	})
	health, err := client.Health(context.Background())
	requireNoError(t, err)
	if health.Status != "ok" {
		t.Errorf("health = %+v", health)
	}
}

func TestVerifyTakeoverIsPossessionOnly(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/takeover/verify" {
			t.Errorf("request = %s %s", r.Method, r.URL.Path)
		}
		if token := r.Header.Get("X-Takeover-Token"); token != "tok-secret" {
			t.Errorf("x-takeover-token = %q", token)
		}
		if auth := r.Header.Get("Authorization"); auth != "" {
			t.Errorf("verify sent authorization %q", auth)
		}
		w.WriteHeader(http.StatusForbidden)
		_, _ = w.Write(problemBody("scope-denied", 403))
	})
	_, err := client.VerifyTakeover(context.Background(), "tok-secret")
	apiError := requireError(t, err)
	if apiError.Kind != "forbidden" || apiError.Status != 403 {
		t.Errorf("error = %+v", apiError)
	}
	if !strings.Contains(apiError.Error(), "detail text") {
		t.Errorf("message = %q", apiError.Error())
	}
}

func TestCreateOperationSendsIdempotencyKey(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if key := r.Header.Get("Idempotency-Key"); key != "idem-7" {
			t.Errorf("idempotency-key = %q", key)
		}
		var body map[string]any
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Errorf("body decode: %v", err)
		}
		if body["kind"] != "send_message" {
			t.Errorf("body = %v", body)
		}
		_, _ = w.Write([]byte(`{"operation":{"id":"op-1","kind":"send_message","state":"PENDING","created_at":1},"replayed":false}`))
	})
	created, err := client.CreateOperation(context.Background(), "idem-7", CreateOperation{Id: "op-1", Kind: "send_message"})
	requireNoError(t, err)
	if created.Replayed {
		t.Error("replayed should be false")
	}
	if created.Operation.State != "PENDING" {
		t.Errorf("state = %q", created.Operation.State)
	}
}

func TestQueryParameters(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		query := r.URL.Query()
		if query.Get("cursor") != "c3" || query.Get("limit") != "5" {
			t.Errorf("query = %v", query)
		}
		_, _ = w.Write([]byte(`{"sessions":[],"next_cursor":null}`))
	})
	_, err := client.ListSessions(context.Background(), stringPointer("c3"), int64Pointer(5))
	requireNoError(t, err)
}

func TestQueryParametersOmitted(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if raw := r.URL.RawQuery; raw != "" {
			t.Errorf("raw query = %q", raw)
		}
		_, _ = w.Write([]byte(`{"events":[],"next_cursor":null}`))
	})
	_, err := client.ListEvents(context.Background(), nil, nil)
	requireNoError(t, err)
}

func TestStreamEventsDrainsText(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if accept := r.Header.Get("Accept"); accept != "text/event-stream" {
			t.Errorf("accept = %q", accept)
		}
		if lastID := r.Header.Get("Last-Event-ID"); lastID != "e42" {
			t.Errorf("last-event-id = %q", lastID)
		}
		if r.URL.Query().Get("consumer") != "worker-1" {
			t.Errorf("consumer = %q", r.URL.Query().Get("consumer"))
		}
		_, _ = io.WriteString(w, "event: caught-up\ndata: {}\n")
	})
	text, err := client.StreamEvents(context.Background(), stringPointer("worker-1"), stringPointer("e42"))
	requireNoError(t, err)
	if text != "event: caught-up\ndata: {}\n" {
		t.Errorf("text = %q", text)
	}
}

func TestCloseAttemptPostsNestedPath(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "POST" || r.URL.Path != "/operations/op-1/attempts/at-1/close" {
			t.Errorf("request = %s %s", r.Method, r.URL.Path)
		}
		_, _ = w.Write([]byte(`{"id":"op-1","kind":"send_message","state":"SUCCEEDED","created_at":1}`))
	})
	operation, err := client.CloseAttempt(context.Background(), "op-1", "at-1", CloseAttempt{WorkerId: "w-1", Result: "succeeded"})
	requireNoError(t, err)
	if operation.State != "SUCCEEDED" {
		t.Errorf("state = %q", operation.State)
	}
}

func TestProblemClassification(t *testing.T) {
	cases := []struct {
		slug string
		want string
		code int
	}{
		{"forbidden", "forbidden", 403},
		{"scope-denied", "forbidden", 403},
		{"not-found", "missing", 404},
		{"invalid", "invalid", 422},
		{"invalid-cursor", "invalid", 400},
		{"conflict", "conflict", 409},
		{"unauthorized", "unauthorized", 401},
		{"reauth-required", "reauth", 401},
		{"store-refused", "server", 500},
		{"storage", "server", 500},
	}
	for _, testCase := range cases {
		t.Run(testCase.slug, func(t *testing.T) {
			client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(testCase.code)
				_, _ = w.Write(problemBody(testCase.slug, testCase.code))
			})
			_, err := client.GetAccount(context.Background(), "acc-1")
			apiError := requireError(t, err)
			if apiError.Kind != testCase.want {
				t.Errorf("kind = %q, want %q", apiError.Kind, testCase.want)
			}
			if apiError.Status != testCase.code {
				t.Errorf("status = %d, want %d", apiError.Status, testCase.code)
			}
			if apiError.Problem.Type != problemBase+testCase.slug {
				t.Errorf("type = %q", apiError.Problem.Type)
			}
		})
	}
}

func TestNonProblemBodyBecomesStoreRefused(t *testing.T) {
	client := testClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadGateway)
		_, _ = w.Write([]byte("<html>boom</html>"))
	})
	_, err := client.ListAccounts(context.Background())
	apiError := requireError(t, err)
	if apiError.Kind != "server" {
		t.Errorf("kind = %q", apiError.Kind)
	}
	if apiError.Problem.Type != problemBase+"store-refused" {
		t.Errorf("type = %q", apiError.Problem.Type)
	}
}

func TestClassifyProblemReference(t *testing.T) {
	if got := ClassifyProblem("https://else.example/x"); got != "other" {
		t.Errorf("foreign uri = %q", got)
	}
	if got := ClassifyProblem(problemBase + "nope"); got != "other" {
		t.Errorf("unknown slug = %q", got)
	}
	if got := ClassifyProblem(problemBase + "reauth-required"); got != "reauth" {
		t.Errorf("reauth-required = %q", got)
	}
}

func stringPointer(value string) *string { return &value }
func int64Pointer(value int64) *int64    { return &value }
