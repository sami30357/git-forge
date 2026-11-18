package main

// Hand-rolled JSON writer: deterministic bytes, no reflection, no HTML escaping
// (matching Node's JSON.stringify output for the values this forge produces).

import (
	"bytes"
	"encoding/json"
	"sort"
	"strconv"
	"strings"
)

// appendJSONString appends s as a JSON string literal. Fast path: when the string
// holds no byte needing an escape it is copied verbatim.
func appendJSONString(b []byte, s string) []byte {
	b = append(b, '"')
	start := 0
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c == '"' || c == '\\' || c < 0x20 {
			b = append(b, s[start:i]...)
			switch c {
			case '"':
				b = append(b, '\\', '"')
			case '\\':
				b = append(b, '\\', '\\')
			case '\n':
				b = append(b, '\\', 'n')
			case '\r':
				b = append(b, '\\', 'r')
			case '\t':
				b = append(b, '\\', 't')
			case '\b':
				b = append(b, '\\', 'b')
			case '\f':
				b = append(b, '\\', 'f')
			default:
				b = append(b, '\\', 'u', '0', '0', hexDigit(c>>4), hexDigit(c&0xf))
			}
			start = i + 1
		}
	}
	b = append(b, s[start:]...)
	return append(b, '"')
}

func hexDigit(v byte) byte {
	if v < 10 {
		return '0' + v
	}
	return 'a' + v - 10
}

// jw is a small JSON object/array builder over a byte slice.
type jw struct{ b []byte }

func (w *jw) raw(s string) { w.b = append(w.b, s...) }
func (w *jw) str(s string) { w.b = appendJSONString(w.b, s) }
func (w *jw) int(n int)    { w.b = strconv.AppendInt(w.b, int64(n), 10) }
func (w *jw) int64(n int64) {
	w.b = strconv.AppendInt(w.b, n, 10)
}
func (w *jw) boolean(v bool) {
	if v {
		w.b = append(w.b, "true"...)
	} else {
		w.b = append(w.b, "false"...)
	}
}
func (w *jw) null() { w.b = append(w.b, "null"...)}
func (w *jw) bytes() []byte { return w.b }

// jsonString renders one value as JSON text (used for MCP structured text payloads
// and other spots that need a serialized value). HTML escaping is off so the
// bytes match what the Node forge produced.
func jsonString(v any) string {
	if v == nil {
		return "null"
	}
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		return "null"
	}
	return strings.TrimRight(buf.String(), "\n")
}

// jobj is an insertion-ordered JSON object (GraphQL/MCP responses are built
// as trees of jobj / []any / scalars, then serialized once).
type jobj struct {
	keys []string
	vals map[string]any
}

func newJobj() *jobj { return &jobj{vals: make(map[string]any)} }

func (o *jobj) set(key string, v any) {
	if _, ok := o.vals[key]; !ok {
		o.keys = append(o.keys, key)
	}
	o.vals[key] = v
}

func (o *jobj) get(key string) (any, bool) {
	v, ok := o.vals[key]
	return v, ok
}

// writeJSONValue writes a generic value as compact JSON: nil, bool, string,
// int, int64, float64, []any, *jobj, map[string]any (sorted keys), or
// json.RawMessage (pre-serialized bytes, embedded verbatim).
func writeJSONValue(w *jw, v any) {
	switch t := v.(type) {
	case nil:
		w.null()
	case bool:
		w.boolean(t)
	case string:
		w.str(t)
	case int:
		w.int(t)
	case int64:
		w.int64(t)
	case float64:
		w.raw(strconv.FormatFloat(t, 'g', -1, 64))
	case []any:
		w.raw(`[`)
		for i, item := range t {
			if i > 0 {
				w.raw(`,`)
			}
			writeJSONValue(w, item)
		}
		w.raw(`]`)
	case *jobj:
		w.raw(`{`)
		for i, k := range t.keys {
			if i > 0 {
				w.raw(`,`)
			}
			w.str(k)
			w.raw(`:`)
			writeJSONValue(w, t.vals[k])
		}
		w.raw(`}`)
	case map[string]any:
		keys := make([]string, 0, len(t))
		for k := range t {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		w.raw(`{`)
		for i, k := range keys {
			if i > 0 {
				w.raw(`,`)
			}
			w.str(k)
			w.raw(`:`)
			writeJSONValue(w, t[k])
		}
		w.raw(`}`)
	case json.RawMessage:
		w.raw(string(t))
	default:
		w.raw(jsonString(v))
	}
}

// fastParseObject scans a flat JSON object and extracts its top-level
// members without reflection. It only succeeds on inputs it fully
// understands (string/bool/null/number values, string arrays); anything else
// returns handled=false and the caller falls back to encoding/json, which
// decides validity. This keeps behavior identical (duplicate keys keep the
// last value, like encoding/json for maps) while skipping the reflection
// overhead on the hot POST bodies (the write workload is 200 sequential
// issue creates whose bodies are flat {"title","body"} objects).
func fastParseObject(body []byte) (map[string]any, bool) {
	p := &fastParser{b: body}
	p.skipWS()
	if p.eof() || p.b[p.i] != '{' {
		return nil, false
	}
	p.i++
	out := make(map[string]any)
	p.skipWS()
	if !p.eof() && p.b[p.i] == '}' {
		p.i++
		p.skipWS()
		if p.eof() {
			return out, true
		}
		return nil, false
	}
	for {
		p.skipWS()
		key, ok := p.scanString()
		if !ok {
			return nil, false
		}
		p.skipWS()
		if p.eof() || p.b[p.i] != ':' {
			return nil, false
		}
		p.i++
		p.skipWS()
		val, ok := p.scanValue()
		if !ok {
			return nil, false
		}
		out[key] = val
		p.skipWS()
		if p.eof() {
			return nil, false
		}
		switch p.b[p.i] {
		case ',':
			p.i++
		case '}':
			p.i++
			p.skipWS()
			if p.eof() {
				return out, true
			}
			return nil, false
		default:
			return nil, false
		}
	}
}

type fastParser struct {
	b []byte
	i int
}

func (p *fastParser) eof() bool { return p.i >= len(p.b) }

func (p *fastParser) skipWS() {
	for p.i < len(p.b) {
		switch p.b[p.i] {
		case ' ', '\t', '\n', '\r':
			p.i++
		default:
			return
		}
	}
}

// scanString reads a JSON string literal starting at the current '"'.
// It decodes the simple escapes inline; any \u escape or raw control byte
// bails to the fallback parser.
func (p *fastParser) scanString() (string, bool) {
	if p.eof() || p.b[p.i] != '"' {
		return "", false
	}
	p.i++
	var out []byte
	start := p.i
	flush := func(end int) {
		if end > start {
			out = append(out, p.b[start:end]...)
		}
	}
	for p.i < len(p.b) {
		c := p.b[p.i]
		switch {
		case c == '"':
			flush(p.i)
			p.i++
			return string(out), true
		case c == '\\':
			flush(p.i)
			p.i++
			if p.eof() {
				return "", false
			}
			esc := p.b[p.i]
			p.i++
			switch esc {
			case '"':
				out = append(out, '"')
			case '\\':
				out = append(out, '\\')
			case '/':
				out = append(out, '/')
			case 'b':
				out = append(out, '\b')
			case 'f':
				out = append(out, '\f')
			case 'n':
				out = append(out, '\n')
			case 'r':
				out = append(out, '\r')
			case 't':
				out = append(out, '\t')
			default:
				// \uXXXX (and anything else) -> let encoding/json decide.
				return "", false
			}
			start = p.i
		case c < 0x20:
			// Raw control byte inside a string is invalid JSON.
			return "", false
		default:
			p.i++
		}
	}
	return "", false
}

func (p *fastParser) scanValue() (any, bool) {
	if p.eof() {
		return nil, false
	}
	switch c := p.b[p.i]; {
	case c == '"':
		return p.scanString()
	case c == 't':
		if p.hasPrefix("true") {
			p.i += 4
			return true, true
		}
		return nil, false
	case c == 'f':
		if p.hasPrefix("false") {
			p.i += 5
			return false, true
		}
		return nil, false
	case c == 'n':
		if p.hasPrefix("null") {
			p.i += 4
			return nil, true
		}
		return nil, false
	case c == '[':
		return p.scanStringArray()
	case c == '-' || (c >= '0' && c <= '9'):
		return p.scanNumber()
	default:
		return nil, false
	}
}

func (p *fastParser) hasPrefix(s string) bool {
	return len(p.b)-p.i >= len(s) && string(p.b[p.i:p.i+len(s)]) == s
}

func (p *fastParser) scanStringArray() (any, bool) {
	p.i++ // '['
	out := []any{}
	p.skipWS()
	if !p.eof() && p.b[p.i] == ']' {
		p.i++
		return out, true
	}
	for {
		p.skipWS()
		s, ok := p.scanString()
		if !ok {
			return nil, false
		}
		out = append(out, s)
		p.skipWS()
		if p.eof() {
			return nil, false
		}
		switch p.b[p.i] {
		case ',':
			p.i++
		case ']':
			p.i++
			return out, true
		default:
			return nil, false
		}
	}
}

// scanNumber reads a strict JSON number literal and converts it the way
// encoding/json does (float64 via ParseFloat). Anything off-grammar bails.
func (p *fastParser) scanNumber() (any, bool) {
	start := p.i
	if p.b[p.i] == '-' {
		p.i++
		if p.eof() {
			return nil, false
		}
	}
	if p.eof() {
		return nil, false
	}
	if p.b[p.i] == '0' {
		p.i++
	} else if p.b[p.i] >= '1' && p.b[p.i] <= '9' {
		for !p.eof() && p.b[p.i] >= '0' && p.b[p.i] <= '9' {
			p.i++
		}
	} else {
		return nil, false
	}
	if !p.eof() && p.b[p.i] == '.' {
		p.i++
		if p.eof() || p.b[p.i] < '0' || p.b[p.i] > '9' {
			return nil, false
		}
		for !p.eof() && p.b[p.i] >= '0' && p.b[p.i] <= '9' {
			p.i++
		}
	}
	if !p.eof() && (p.b[p.i] == 'e' || p.b[p.i] == 'E') {
		p.i++
		if !p.eof() && (p.b[p.i] == '+' || p.b[p.i] == '-') {
			p.i++
		}
		if p.eof() || p.b[p.i] < '0' || p.b[p.i] > '9' {
			return nil, false
		}
		for !p.eof() && p.b[p.i] >= '0' && p.b[p.i] <= '9' {
			p.i++
		}
	}
	f, err := strconv.ParseFloat(string(p.b[start:p.i]), 64)
	if err != nil {
		return nil, false
	}
	return f, true
}

// parseJSONBody mirrors the Node forge's parseJsonBody: an empty body is {},
// malformed JSON fails, and a well-formed non-object JSON value is silently {}.
// The fast path handles flat objects; anything else goes through encoding/json.
func parseJSONBody(body []byte) (map[string]any, bool) {
	if len(body) == 0 {
		return map[string]any{}, true
	}
	if m, ok := fastParseObject(body); ok {
		return m, true
	}
	var parsed any
	if err := json.Unmarshal(body, &parsed); err != nil {
		return nil, false
	}
	if m, ok := parsed.(map[string]any); ok {
		return m, true
	}
	return map[string]any{}, true
}

// jsNumber mirrors JavaScript's Number() for the small numeric strings this forge
// sees (issue/PR numbers, pagination, pull_number): trims space, '' is 0, hex
// 0x.. works, anything else is NaN.
func jsNumber(s string) (float64, bool) {
	t := trimSpace(s)
	if t == "" {
		return 0, true
	}
	if len(t) > 2 && t[0] == '0' && (t[1] == 'x' || t[1] == 'X') {
		n, err := strconv.ParseInt(t[2:], 16, 64)
		if err != nil {
			return 0, false
		}
		return float64(n), true
	}
	f, err := strconv.ParseFloat(t, 64)
	if err != nil {
		return 0, false
	}
	return f, true
}

func trimSpace(s string) string {
	start := 0
	for start < len(s) && isSpaceByte(s[start]) {
		start++
	}
	end := len(s)
	for end > start && isSpaceByte(s[end-1]) {
		end--
	}
	return s[start:end]
}

func isSpaceByte(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f'
}

// jsonStringOf renders a string value as JSON (for building small ad-hoc bodies).
func jsonStringOf(s string) string {
	return string(appendJSONString(nil, s))
}
