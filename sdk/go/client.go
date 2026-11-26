// GENERATED FILE — scripts/generate_sdk.py from crates/server/tests/fixtures/openapi.json. Do not edit by hand.
package fishpond

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
)

const problemBase = "https://fishpond.dev/problems/"

// Problem is one RFC 9457 problem document.
type Problem struct {
	Type   string `json:"type"`
	Title  string `json:"title"`
	Status int    `json:"status"`
	Detail string `json:"detail"`
}

// Error is one failed call: the parsed problem plus its class.
type Error struct {
	Kind    string
	Status  int
	Problem Problem
}

func (e *Error) Error() string {
	detail := e.Problem.Detail
	if detail == "" {
		detail = e.Problem.Title
	}
	return fmt.Sprintf("%s: %s", e.Kind, detail)
}

// ClassifyProblem maps a problem type URI to the SDK's failure classes.
func ClassifyProblem(typeURI string) string {
	if !strings.HasPrefix(typeURI, problemBase) {
		return "other"
	}
	tail := typeURI
	if idx := strings.LastIndex(typeURI, "/"); idx >= 0 {
		tail = typeURI[idx+1:]
	}
	switch tail {
	case "conflict":
		return "conflict"
	case "forbidden":
		return "forbidden"
	case "invalid":
		return "invalid"
	case "invalid-cursor":
		return "invalid"
	case "not-found":
		return "missing"
	case "reauth-required":
		return "reauth"
	case "scope-denied":
		return "forbidden"
	case "storage":
		return "server"
	case "store-refused":
		return "server"
	case "unauthorized":
		return "unauthorized"
	default:
		return "other"
	}
}

// Account mirrors the contract schema of the same name.
type Account struct {
	Id        string  `json:"id"`
	Platform  string  `json:"platform"`
	State     string  `json:"state"`
	StateNote *string `json:"state_note,omitempty"`
	UpdatedAt int64   `json:"updated_at"`
}

// AccountList mirrors the contract schema of the same name.
type AccountList struct {
	Accounts []Account `json:"accounts"`
}

// AuthSession mirrors the contract schema of the same name.
type AuthSession struct {
	AccountId    string  `json:"account_id"`
	CreatedAt    int64   `json:"created_at"`
	CredentialId *string `json:"credential_id,omitempty"`
	ExpiresAt    *int64  `json:"expires_at,omitempty"`
	Id           string  `json:"id"`
	Platform     string  `json:"platform"`
	State        string  `json:"state"`
	StateNote    *string `json:"state_note,omitempty"`
	UpdatedAt    int64   `json:"updated_at"`
}

// ClaimOperation mirrors the contract schema of the same name.
type ClaimOperation struct {
	AttemptId  string `json:"attempt_id"`
	DeadlineAt int64  `json:"deadline_at"`
	WorkerId   string `json:"worker_id"`
}

// ClaimedOperation mirrors the contract schema of the same name.
type ClaimedOperation struct {
	Attempt   ClaimedOperationAttempt `json:"attempt"`
	Operation Operation               `json:"operation"`
}

// CloseAttempt mirrors the contract schema of the same name.
type CloseAttempt struct {
	Result   string `json:"result"`
	WorkerId string `json:"worker_id"`
}

// CreateOperation mirrors the contract schema of the same name.
type CreateOperation struct {
	Id   string `json:"id"`
	Kind string `json:"kind"`
}

// CreatedOperation mirrors the contract schema of the same name.
type CreatedOperation struct {
	Operation Operation `json:"operation"`
	Replayed  bool      `json:"replayed"`
}

// Event mirrors the contract schema of the same name.
type Event struct {
	AccountId  *string `json:"account_id,omitempty"`
	EventType  string  `json:"event_type"`
	Id         string  `json:"id"`
	OccurredAt int64   `json:"occurred_at"`
	Payload    any     `json:"payload,omitempty"`
	Seq        int64   `json:"seq"`
}

// EventPage mirrors the contract schema of the same name.
type EventPage struct {
	Events     []Event `json:"events"`
	NextCursor string  `json:"next_cursor"`
}

// Grant mirrors the contract schema of the same name.
type Grant struct {
	CreatedAt int64  `json:"created_at"`
	ExpiresAt int64  `json:"expires_at"`
	Id        string `json:"id"`
	Operator  string `json:"operator"`
	RevokedAt *int64 `json:"revoked_at,omitempty"`
	TokenHint string `json:"token_hint"`
}

// GrantIssued mirrors the contract schema of the same name.
type GrantIssued struct {
	ExpiresAt int64  `json:"expires_at"`
	GrantId   string `json:"grant_id"`
	Token     string `json:"token"`
}

// GrantList mirrors the contract schema of the same name.
type GrantList struct {
	Grants []Grant `json:"grants"`
}

// Health mirrors the contract schema of the same name.
type Health struct {
	Service string `json:"service"`
	Status  string `json:"status"`
	Version string `json:"version"`
}

// Operation mirrors the contract schema of the same name.
type Operation struct {
	CreatedAt int64  `json:"created_at"`
	Id        string `json:"id"`
	Kind      string `json:"kind"`
	State     string `json:"state"`
}

// OperationPage mirrors the contract schema of the same name.
type OperationPage struct {
	NextCursor string      `json:"next_cursor"`
	Operations []Operation `json:"operations"`
}

// RegisterAccount mirrors the contract schema of the same name.
type RegisterAccount struct {
	Id       string `json:"id"`
	Platform string `json:"platform"`
}

// ReleaseResult mirrors the contract schema of the same name.
type ReleaseResult struct {
	Released bool `json:"released"`
}

// SessionPage mirrors the contract schema of the same name.
type SessionPage struct {
	NextCursor string        `json:"next_cursor"`
	Sessions   []AuthSession `json:"sessions"`
}

// SetAccountState mirrors the contract schema of the same name.
type SetAccountState struct {
	Note  *string `json:"note,omitempty"`
	State string  `json:"state"`
}

// StartSession mirrors the contract schema of the same name.
type StartSession struct {
	AccountId string `json:"account_id"`
	ExpiresAt *int64 `json:"expires_at,omitempty"`
	Id        string `json:"id"`
}

// StartTakeover mirrors the contract schema of the same name.
type StartTakeover struct {
	Operator string `json:"operator"`
	TtlMs    *int64 `json:"ttl_ms,omitempty"`
}

// StatusSummary mirrors the contract schema of the same name.
type StatusSummary struct {
	Disabled int64 `json:"disabled"`
	Offline  int64 `json:"offline"`
	Online   int64 `json:"online"`
	Total    int64 `json:"total"`
	Unknown  int64 `json:"unknown"`
}

// TakeoverContext mirrors the contract schema of the same name.
type TakeoverContext struct {
	Account   string `json:"account"`
	ExpiresAt int64  `json:"expires_at"`
	GrantId   string `json:"grant_id"`
	Operator  string `json:"operator"`
	Session   string `json:"session"`
	Tenant    string `json:"tenant"`
}

// TransitionSession mirrors the contract schema of the same name.
type TransitionSession struct {
	Action            string  `json:"action"`
	CredentialId      *string `json:"credential_id,omitempty"`
	CredentialVersion *int64  `json:"credential_version,omitempty"`
	ExpiresAt         *int64  `json:"expires_at,omitempty"`
	Note              *string `json:"note,omitempty"`
}

// ClaimedOperationAttempt mirrors the contract schema of the same name.
type ClaimedOperationAttempt struct {
	AttemptId  string `json:"attempt_id"`
	AttemptNo  int64  `json:"attempt_no"`
	DeadlineAt int64  `json:"deadline_at"`
}

// Client is a typed client for the fishpond control plane.
type Client struct {
	BaseURL string
	HTTP    *http.Client
	Key     func() string
}

// NewClient builds a client; key may be nil for anonymous use.
func NewClient(baseURL string, key func() string) *Client {
	return &Client{BaseURL: baseURL, HTTP: http.DefaultClient, Key: key}
}

// do performs one call and returns the raw 2xx body.
func (c *Client) do(ctx context.Context, method, path string, query url.Values, headers http.Header, body any, bearer bool) ([]byte, error) {
	if query != nil {
		if encoded := query.Encode(); encoded != "" {
			path += "?" + encoded
		}
	}
	var reader io.Reader
	if body != nil {
		encoded, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		reader = bytes.NewReader(encoded)
	}
	request, err := http.NewRequestWithContext(ctx, method, c.BaseURL+path, reader)
	if err != nil {
		return nil, err
	}
	if body != nil {
		request.Header.Set("Content-Type", "application/json")
	}
	for key, values := range headers {
		for _, value := range values {
			request.Header.Add(key, value)
		}
	}
	if bearer && c.Key != nil {
		if key := c.Key(); key != "" {
			request.Header.Set("Authorization", "Bearer "+key)
		}
	}
	response, err := c.HTTP.Do(request)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode >= 300 {
		var problem Problem
		if err := json.NewDecoder(response.Body).Decode(&problem); err != nil || problem.Type == "" {
			problem = Problem{
				Type:   problemBase + "store-refused",
				Title:  "error",
				Status: response.StatusCode,
				Detail: fmt.Sprintf("control plane answered %d with a non-problem body", response.StatusCode),
			}
		}
		return nil, &Error{Kind: ClassifyProblem(problem.Type), Status: response.StatusCode, Problem: problem}
	}
	return io.ReadAll(response.Body)
}

// Account counts by state
func (c *Client) AccountStatus(ctx context.Context) (StatusSummary, error) {
	var out StatusSummary
	payload, err := c.do(ctx, "GET", "/status", nil, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Claim a PENDING operation (CAS to RUNNING with a numbered attempt)
func (c *Client) ClaimOperation(ctx context.Context, id string, body ClaimOperation) (ClaimedOperation, error) {
	var out ClaimedOperation
	payload, err := c.do(ctx, "POST", "/operations/"+url.PathEscape(id)+"/claim", nil, nil, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Settle one attempt (succeeded/failed end the operation; timed-out requeues)
func (c *Client) CloseAttempt(ctx context.Context, id string, attemptId string, body CloseAttempt) (Operation, error) {
	var out Operation
	payload, err := c.do(ctx, "POST", "/operations/"+url.PathEscape(id)+"/attempts/"+url.PathEscape(attemptId)+"/close", nil, nil, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Idempotently create an operation (PENDING)
func (c *Client) CreateOperation(ctx context.Context, idempotencyKey string, body CreateOperation) (CreatedOperation, error) {
	headers := http.Header{}
	headers.Set("Idempotency-Key", idempotencyKey)
	var out CreatedOperation
	payload, err := c.do(ctx, "POST", "/operations", nil, headers, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// One account
func (c *Client) GetAccount(ctx context.Context, id string) (Account, error) {
	var out Account
	payload, err := c.do(ctx, "GET", "/accounts/"+url.PathEscape(id)+"", nil, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// One operation
func (c *Client) GetOperation(ctx context.Context, id string) (Operation, error) {
	var out Operation
	payload, err := c.do(ctx, "GET", "/operations/"+url.PathEscape(id)+"", nil, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// One auth session
func (c *Client) GetSession(ctx context.Context, id string) (AuthSession, error) {
	var out AuthSession
	payload, err := c.do(ctx, "GET", "/sessions/"+url.PathEscape(id)+"", nil, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Liveness probe
func (c *Client) Health(ctx context.Context) (Health, error) {
	var out Health
	payload, err := c.do(ctx, "GET", "/healthz", nil, nil, nil, false)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// List the tenant's accounts (ordered by id)
func (c *Client) ListAccounts(ctx context.Context) (AccountList, error) {
	var out AccountList
	payload, err := c.do(ctx, "GET", "/accounts", nil, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Bounded page of the tenant's event log (cursor pagination)
func (c *Client) ListEvents(ctx context.Context, cursor *string, limit *int64) (EventPage, error) {
	query := url.Values{}
	if cursor != nil {
		query.Set("cursor", *cursor)
	}
	if limit != nil {
		query.Set("limit", strconv.FormatInt(*limit, 10))
	}
	var out EventPage
	payload, err := c.do(ctx, "GET", "/events", query, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// List the tenant's operations (oldest first, cursor pagination)
func (c *Client) ListOperations(ctx context.Context, cursor *string, limit *int64) (OperationPage, error) {
	query := url.Values{}
	if cursor != nil {
		query.Set("cursor", *cursor)
	}
	if limit != nil {
		query.Set("limit", strconv.FormatInt(*limit, 10))
	}
	var out OperationPage
	payload, err := c.do(ctx, "GET", "/operations", query, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Cursor page of the tenant's auth sessions (ordered by created_at, id)
func (c *Client) ListSessions(ctx context.Context, cursor *string, limit *int64) (SessionPage, error) {
	query := url.Values{}
	if cursor != nil {
		query.Set("cursor", *cursor)
	}
	if limit != nil {
		query.Set("limit", strconv.FormatInt(*limit, 10))
	}
	var out SessionPage
	payload, err := c.do(ctx, "GET", "/sessions", query, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// The session's takeover grant audit list (hashes and hints, never raw tokens)
func (c *Client) ListTakeoverGrants(ctx context.Context, id string) (GrantList, error) {
	var out GrantList
	payload, err := c.do(ctx, "GET", "/sessions/"+url.PathEscape(id)+"/takeover", nil, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Register an account (OFFLINE)
func (c *Client) RegisterAccount(ctx context.Context, body RegisterAccount) (Account, error) {
	var out Account
	payload, err := c.do(ctx, "POST", "/accounts", nil, nil, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Release the takeover: the credential dies and automation resumes
func (c *Client) ReleaseTakeover(ctx context.Context, id string, grantId string) (ReleaseResult, error) {
	var out ReleaseResult
	payload, err := c.do(ctx, "DELETE", "/sessions/"+url.PathEscape(id)+"/takeover/"+url.PathEscape(grantId)+"", nil, nil, nil, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// One account state-machine step
func (c *Client) SetAccountState(ctx context.Context, id string, body SetAccountState) (Account, error) {
	var out Account
	payload, err := c.do(ctx, "POST", "/accounts/"+url.PathEscape(id)+"/state", nil, nil, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Start a login session (PENDING)
func (c *Client) StartSession(ctx context.Context, body StartSession) (AuthSession, error) {
	var out AuthSession
	payload, err := c.do(ctx, "POST", "/sessions", nil, nil, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Take the session over and mint the operator credential (the raw token appears exactly once)
func (c *Client) StartTakeover(ctx context.Context, id string, body StartTakeover) (GrantIssued, error) {
	var out GrantIssued
	payload, err := c.do(ctx, "POST", "/sessions/"+url.PathEscape(id)+"/takeover", nil, nil, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// SSE stream of the tenant's event log
func (c *Client) StreamEvents(ctx context.Context, consumer *string, lastEventID *string) (string, error) {
	query := url.Values{}
	if consumer != nil {
		query.Set("consumer", *consumer)
	}
	headers := http.Header{}
	if lastEventID != nil {
		headers.Set("Last-Event-ID", *lastEventID)
	}
	headers.Set("Accept", "text/event-stream")
	payload, err := c.do(ctx, "GET", "/events/stream", query, headers, nil, true)
	if err != nil {
		return "", err
	}
	return string(payload), nil
}

// One named state step (confirm/authorize/expire/revoke/fail/take-over/resume)
func (c *Client) TransitionSession(ctx context.Context, id string, body TransitionSession) (AuthSession, error) {
	var out AuthSession
	payload, err := c.do(ctx, "POST", "/sessions/"+url.PathEscape(id)+"/transition", nil, nil, body, true)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}

// Verify an operator takeover token by possession (X-Takeover-Token header; no bearer key)
func (c *Client) VerifyTakeover(ctx context.Context, xTakeoverToken string) (TakeoverContext, error) {
	headers := http.Header{}
	headers.Set("X-Takeover-Token", xTakeoverToken)
	var out TakeoverContext
	payload, err := c.do(ctx, "POST", "/takeover/verify", nil, headers, nil, false)
	if err != nil {
		return out, err
	}
	if err := json.Unmarshal(payload, &out); err != nil {
		return out, err
	}
	return out, nil
}
