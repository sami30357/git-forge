package main

// Git object model and packfiles, ported from the Node forge with identical
// semantics: plain objects only, SHA-1 ids, pkt-line framing, OFS/REF delta
// resolution on read.

import (
	"bytes"
	"compress/zlib"
	"crypto/sha1"
	"encoding/hex"
	"errors"
	"fmt"
	"hash"
	"io"
	"sort"
	"sync"
)

type gitObjectType string

const (
	objCommit gitObjectType = "commit"
	objTree   gitObjectType = "tree"
	objBlob   gitObjectType = "blob"
	objTag    gitObjectType = "tag"
)

type gitObject struct {
	typ  gitObjectType
	data []byte
}

var typeCode = map[gitObjectType]byte{objCommit: 1, objTree: 2, objBlob: 3, objTag: 4}
var codeType = map[byte]gitObjectType{1: objCommit, 2: objTree, 3: objBlob, 4: objTag}

const zeroOID = "0000000000000000000000000000000000000000"
const emptyTreeOID = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"

func sha1Hex(data []byte) string {
	sum := sha1.Sum(data)
	return hex.EncodeToString(sum[:])
}

// sha1Pool reuses hashers: hashObject runs on every pushed object and
// sha1.New allocates a fresh digest state each call.
var sha1Pool = sync.Pool{New: func() any { return sha1.New() }}

func hashObject(typ gitObjectType, data []byte) string {
	// Header "<type> <len>\0" built in a stack buffer (no fmt.Sprintf).
	var hdr [32]byte
	n := copy(hdr[:], string(typ))
	hdr[n] = ' '
	n++
	// Append the decimal length.
	v := len(data)
	var num [20]byte
	i := len(num)
	if v == 0 {
		i--
		num[i] = '0'
	} else {
		for v > 0 {
			i--
			num[i] = byte('0' + v%10)
			v /= 10
		}
	}
	n += copy(hdr[n:], num[i:])
	hdr[n] = 0
	n++
	h := sha1Pool.Get().(hash.Hash)
	h.Reset()
	h.Write(hdr[:n])
	h.Write(data)
	var sum [20]byte
	h.Sum(sum[:0])
	sha1Pool.Put(h)
	var hexbuf [40]byte
	hex.Encode(hexbuf[:], sum[:])
	return string(hexbuf[:])
}

func isOID(s string) bool {
	if len(s) != 40 {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

// isOIDBytes is isOID over a byte slice (avoids a string conversion while
// validating a pack command).
func isOIDBytes(b []byte) bool {
	if len(b) != 40 {
		return false
	}
	for i := 0; i < len(b); i++ {
		c := b[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

// ---------------------------------------------------------------------------
// pkt-lines
// ---------------------------------------------------------------------------

const pktMax = 65520

func pktLine(payload []byte) []byte {
	if len(payload)+4 > pktMax {
		panic("pkt-line payload too long")
	}
	out := make([]byte, 4+len(payload))
	hex4(out[:4], len(payload)+4)
	copy(out[4:], payload)
	return out
}

// appendPktLine appends one pkt-line for the string line plus its "\n" to
// dst, without allocating the payload. Used by the receive-pack report.
func appendPktLine(dst []byte, line string) []byte {
	n := len(line) + 1 + 4
	var h [4]byte
	hex4(h[:], n)
	dst = append(dst, h[:]...)
	dst = append(dst, line...)
	dst = append(dst, '\n')
	return dst
}

// appendPktLineBytes appends one pkt-line for the byte payload to dst.
func appendPktLineBytes(dst, payload []byte) []byte {
	n := len(payload) + 4
	var h [4]byte
	hex4(h[:], n)
	dst = append(dst, h[:]...)
	dst = append(dst, payload...)
	return dst
}

func hex4(dst []byte, n int) {
	const digits = "0123456789abcdef"
	dst[0] = digits[(n>>12)&0xf]
	dst[1] = digits[(n>>8)&0xf]
	dst[2] = digits[(n>>4)&0xf]
	dst[3] = digits[n&0xf]
}

func pktFlush() []byte { return []byte("0000") }
func pktDelim() []byte { return []byte("0001") }

func pktLines(items [][]byte) []byte {
	var out []byte
	for _, item := range items {
		out = append(out, item...)
	}
	return out
}

// pktLineStr builds one pkt-line from a string payload.
func pktLineStr(s string) []byte { return pktLine([]byte(s)) }

type pktPacket struct {
	kind    byte // 'f' flush, 'd' delim, 'D' data
	payload []byte
}

// decodePktLines decodes pkt-lines from the head of buf. It stops at the first
// flush (returning the bytes after it as rest) or at the end of the buffer.
func decodePktLines(buf []byte) (packets []pktPacket, rest []byte) {
	offset := 0
	for offset+4 <= len(buf) {
		header := buf[offset : offset+4]
		if !isHex4(header) {
			break
		}
		length := hexVal(header)
		if length == 0 {
			packets = append(packets, pktPacket{kind: 'f'})
			return packets, buf[offset+4:]
		}
		if length == 1 {
			packets = append(packets, pktPacket{kind: 'd'})
			offset += 4
			continue
		}
		if length == 2 || length == 3 || length > pktMax || offset+length > len(buf) {
			break
		}
		packets = append(packets, pktPacket{kind: 'D', payload: buf[offset+4 : offset+length]})
		offset += length
	}
	return packets, buf[offset:]
}

func isHex4(b []byte) bool {
	for _, c := range b {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F') {
			return false
		}
	}
	return true
}

func hexVal(b []byte) int {
	n := 0
	for _, c := range b {
		n <<= 4
		switch {
		case c >= '0' && c <= '9':
			n += int(c - '0')
		case c >= 'a' && c <= 'f':
			n += int(c-'a') + 10
		case c >= 'A' && c <= 'F':
			n += int(c-'A') + 10
		}
	}
	return n
}

func packetText(p pktPacket) string {
	if p.kind == 'D' {
		return string(p.payload)
	}
	return ""
}

// ---------------------------------------------------------------------------
// tree / commit / tag
// ---------------------------------------------------------------------------

type treeEntry struct {
	mode string
	name string
	oid  string
}

func parseTree(data []byte) ([]treeEntry, error) {
	var entries []treeEntry
	offset := 0
	for offset < len(data) {
		space := bytes.IndexByte(data[offset:], ' ')
		if space < 0 {
			return nil, errors.New("bad tree object")
		}
		space += offset
		nul := bytes.IndexByte(data[space+1:], 0)
		if nul < 0 {
			return nil, errors.New("bad tree object")
		}
		nul += space + 1
		if nul+21 > len(data) {
			return nil, errors.New("bad tree object")
		}
		entries = append(entries, treeEntry{
			mode: string(data[offset:space]),
			name: string(data[space+1 : nul]),
			oid:  hex.EncodeToString(data[nul+1 : nul+21]),
		})
		offset = nul + 21
	}
	return entries, nil
}

func compareTreeNames(a, b treeEntry) int {
	an := a.name
	if a.mode == "40000" {
		an += "/"
	}
	bn := b.name
	if b.mode == "40000" {
		bn += "/"
	}
	return bytes.Compare([]byte(an), []byte(bn))
}

func buildTree(entries []treeEntry) []byte {
	sorted := append([]treeEntry(nil), entries...)
	sort.Slice(sorted, func(i, j int) bool { return compareTreeNames(sorted[i], sorted[j]) < 0 })
	var out []byte
	for _, e := range sorted {
		out = append(out, []byte(e.mode+" "+e.name+"\x00")...)
		raw, _ := hex.DecodeString(e.oid)
		out = append(out, raw...)
	}
	return out
}

type commitView struct {
	tree      string
	parents   []string
	author    string
	committer string
	message   string
}

func parseCommit(data []byte) commitView {
	blank := bytes.Index(data, []byte("\n\n"))
	headerEnd := len(data)
	if blank >= 0 {
		headerEnd = blank
	}
	var message string
	if blank >= 0 {
		message = string(data[blank+2:])
	}
	cv := commitView{}
	for _, line := range bytes.Split(data[:headerEnd], []byte("\n")) {
		if len(line) == 0 {
			continue
		}
		space := bytes.IndexByte(line, ' ')
		key := string(line)
		value := ""
		if space >= 0 {
			key = string(line[:space])
			value = string(line[space+1:])
		}
		switch key {
		case "tree":
			cv.tree = value
		case "parent":
			cv.parents = append(cv.parents, value)
		case "author":
			cv.author = value
		case "committer":
			cv.committer = value
		}
	}
	cv.message = message
	return cv
}

func buildCommit(tree string, parents []string, message string, timestamp int64) []byte {
	var b bytes.Buffer
	b.WriteString("tree " + tree + "\n")
	for _, p := range parents {
		b.WriteString("parent " + p + "\n")
	}
	b.WriteString("author " + ident + " " + itoa(timestamp) + " +0000\n")
	b.WriteString("committer " + ident + " " + itoa(timestamp) + " +0000\n")
	b.WriteString("\n")
	b.WriteString(message)
	return b.Bytes()
}

func buildTag(object string, typ gitObjectType, tag, message string, timestamp int64) []byte {
	return []byte(fmt.Sprintf("object %s\ntype %s\ntag %s\ntagger %s %d +0000\n\n%s",
		object, string(typ), tag, ident, timestamp, message))
}

type tagView struct {
	object  string
	typ     string
	tag     string
	message string
}

func parseTag(data []byte) tagView {
	blank := bytes.Index(data, []byte("\n\n"))
	headerEnd := len(data)
	if blank >= 0 {
		headerEnd = blank
	}
	tv := tagView{}
	for _, line := range bytes.Split(data[:headerEnd], []byte("\n")) {
		if len(line) == 0 {
			continue
		}
		space := bytes.IndexByte(line, ' ')
		if space < 0 {
			continue
		}
		key := string(line[:space])
		value := string(line[space+1:])
		switch key {
		case "object":
			tv.object = value
		case "type":
			tv.typ = value
		case "tag":
			tv.tag = value
		}
	}
	if blank >= 0 {
		tv.message = string(data[blank+2:])
	}
	return tv
}

func itoa(n int64) string {
	if n == 0 {
		return "0"
	}
	neg := n < 0
	if neg {
		n = -n
	}
	var buf [20]byte
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte('0' + n%10)
		n /= 10
	}
	if neg {
		i--
		buf[i] = '-'
	}
	return string(buf[i:])
}

// ---------------------------------------------------------------------------
// packfiles
// ---------------------------------------------------------------------------

func writePackObjectHeader(typ gitObjectType, size int) []byte {
	code := typeCode[typ]
	var out []byte
	remaining := size
	first := (code << 4) | byte(remaining&0x0f)
	remaining >>= 4
	for remaining > 0 {
		out = append(out, first|0x80)
		first = byte(remaining & 0x7f)
		remaining >>= 7
	}
	out = append(out, first)
	return out
}

// zlibWriterPool reuses compressors across pack builds: a fresh zlib writer
// allocates ~256 KB of deflate state (window + hash tables) on first use,
// which dwarfed the actual compression of the small objects in a pack.
var zlibWriterPool = sync.Pool{New: func() any {
	zw, _ := zlib.NewWriterLevel(&bytes.Buffer{}, zlib.BestSpeed)
	return zw
}}

func writePack(objects []gitObject) []byte {
	var body bytes.Buffer
	body.WriteString("PACK")
	body.Write([]byte{0, 0, 0, 2})
	body.Write([]byte{byte(len(objects) >> 24), byte(len(objects) >> 16), byte(len(objects) >> 8), byte(len(objects))})
	// One pooled compressor for the whole pack, reset per object.
	// BestSpeed: the pack crosses loopback, so size barely matters and
	// level 1 is far cheaper than the default level 6.
	zw := zlibWriterPool.Get().(*zlib.Writer)
	defer zlibWriterPool.Put(zw)
	var zb bytes.Buffer
	for _, obj := range objects {
		body.Write(writePackObjectHeader(obj.typ, len(obj.data)))
		zb.Reset()
		zw.Reset(&zb)
		zw.Write(obj.data)
		zw.Close()
		body.Write(zb.Bytes())
	}
	sum := sha1.Sum(body.Bytes())
	out := append(body.Bytes(), sum[:]...)
	return out
}

// inflater reuses one zlib reader across a pack's entries instead of
// allocating a fresh decompressor (with its window and Huffman tables)
// per object.
type inflater struct {
	rc io.ReadCloser
	rs zlib.Resetter
}

func (in *inflater) inflate(pack []byte, offset, end, sizeHint int) ([]byte, int, error) {
	br := bytes.NewReader(pack[offset:end])
	if in.rc == nil {
		rc, err := zlib.NewReader(br)
		if err != nil {
			return nil, 0, err
		}
		in.rc = rc
		if rs, ok := rc.(zlib.Resetter); ok {
			in.rs = rs
		}
	} else if in.rs != nil {
		if err := in.rs.Reset(br, nil); err != nil {
			return nil, 0, err
		}
	} else {
		in.rc.Close()
		rc, err := zlib.NewReader(br)
		if err != nil {
			return nil, 0, err
		}
		in.rc = rc
		if rs, ok := rc.(zlib.Resetter); ok {
			in.rs = rs
		}
	}
	// The header already told us the inflated size: read exactly that much
	// into one allocation instead of ReadAll's grow-by-doubling.
	data := make([]byte, sizeHint)
	if _, err := io.ReadFull(in.rc, data); err != nil {
		return nil, 0, err
	}
	// A well-formed entry ends here; anything more is a size mismatch.
	var one [1]byte
	if _, err := in.rc.Read(one[:]); err != io.EOF {
		if err == nil {
			return nil, 0, errors.New("pack entry size mismatch")
		}
		return nil, 0, err
	}
	consumed := (end - offset) - br.Len()
	return data, consumed, nil
}

func readVarintSize(buf []byte, start int) (size int, next int, first byte, err error) {
	if start >= len(buf) {
		return 0, 0, 0, errors.New("pack truncated")
	}
	b := buf[start]
	first = b
	size = int(b & 0x0f)
	shift := uint(4)
	offset := start + 1
	for b&0x80 != 0 {
		if offset >= len(buf) {
			return 0, 0, 0, errors.New("pack truncated")
		}
		b = buf[offset]
		offset++
		size += int(b&0x7f) << shift
		shift += 7
	}
	return size, offset, first, nil
}

func readOfsDeltaBase(buf []byte, start int) (baseOffset int, next int, err error) {
	if start >= len(buf) {
		return 0, 0, errors.New("pack truncated")
	}
	b := buf[start]
	offset := start + 1
	value := int(b & 0x7f)
	for b&0x80 != 0 {
		if offset >= len(buf) {
			return 0, 0, errors.New("pack truncated")
		}
		b = buf[offset]
		offset++
		value = (value+1)*128 + int(b&0x7f)
	}
	return value, offset, nil
}

func applyDelta(base, delta []byte) ([]byte, error) {
	offset := 0
	readSize := func() (int, error) {
		value := 0
		shift := uint(0)
		for {
			if offset >= len(delta) {
				return 0, errors.New("delta truncated")
			}
			b := delta[offset]
			offset++
			value += int(b&0x7f) << shift
			shift += 7
			if b&0x80 == 0 {
				break
			}
		}
		return value, nil
	}
	sourceSize, err := readSize()
	if err != nil {
		return nil, err
	}
	targetSize, err := readSize()
	if err != nil {
		return nil, err
	}
	if sourceSize != len(base) {
		return nil, errors.New("delta base size mismatch")
	}
	out := make([]byte, targetSize)
	written := 0
	for offset < len(delta) {
		op := delta[offset]
		offset++
		if op&0x80 != 0 {
			copyOffset := 0
			copySize := 0
			shift := uint(0)
			for bit := 0; bit < 4; bit++ {
				if op&(1<<uint(bit)) != 0 {
					if offset >= len(delta) {
						return nil, errors.New("delta truncated")
					}
					copyOffset += int(delta[offset]) << shift
					offset++
				}
				shift += 8
			}
			shift = 0
			for bit := 4; bit < 7; bit++ {
				if op&(1<<uint(bit)) != 0 {
					if offset >= len(delta) {
						return nil, errors.New("delta truncated")
					}
					copySize += int(delta[offset]) << shift
					offset++
				}
				shift += 8
			}
			if copySize == 0 {
				copySize = 0x10000
			}
			if copyOffset+copySize > len(base) || written+copySize > len(out) {
				return nil, errors.New("delta copy out of range")
			}
			copy(out[written:], base[copyOffset:copyOffset+copySize])
			written += copySize
		} else if op != 0 {
			literal := int(op & 0x7f)
			if offset+literal > len(delta) || written+literal > len(out) {
				return nil, errors.New("delta truncated")
			}
			copy(out[written:], delta[offset:offset+literal])
			offset += literal
			written += literal
		} else {
			return nil, errors.New("delta opcode 0")
		}
	}
	if written != targetSize {
		return nil, errors.New("delta size mismatch")
	}
	return out, nil
}

type rawEntry struct {
	offset    int
	typeCode  byte
	size      int
	data      []byte
	baseOff   int
	baseOID   string
	hasBase   bool
	resolved  *resolvedObj
}

type resolvedObj struct {
	typ  gitObjectType
	data []byte
	oid  string
}

// readPack reads a packfile, resolving OFS/REF deltas (REF bases may come from
// the store lookup). Returns oid -> object.
func readPack(pack []byte, lookup func(string) *gitObject) (map[string]gitObject, error) {
	if objs, flat, err := readPackFlat(pack); err != nil {
		return nil, err
	} else if flat {
		return objs, nil
	}
	return readPackGeneral(pack, lookup)
}

// readPackFlat is the fast path for delta-free packs (every pack the suite's
// client sends: plain blob/tree/commit objects, no type-6/7 entries). It makes
// one pass with a single result map — no entry list, no offset map, no
// resolve closure, no per-entry heap structs. It returns flat=false the
// moment it meets a delta entry, so the general path handles those exactly
// as before. Any other error is final (the general path would return it too).
func readPackFlat(pack []byte) (map[string]gitObject, bool, error) {
	if len(pack) < 32 {
		return nil, true, errors.New("pack too short")
	}
	if string(pack[0:4]) != "PACK" {
		return nil, true, errors.New("not a pack")
	}
	version := int(pack[4])<<24 | int(pack[5])<<16 | int(pack[6])<<8 | int(pack[7])
	if version != 2 && version != 3 {
		return nil, true, fmt.Errorf("unsupported pack version %d", version)
	}
	declaredCount := int(pack[8])<<24 | int(pack[9])<<16 | int(pack[10])<<8 | int(pack[11])
	trailerStart := len(pack) - 20
	objects := make(map[string]gitObject, declaredCount)
	var inf inflater
	offset := 12
	for i := 0; i < declaredCount; i++ {
		size, next, first, err := readVarintSize(pack, offset)
		if err != nil {
			return nil, true, err
		}
		offset = next
		tc := (first >> 4) & 0x07
		if tc == 6 || tc == 7 {
			return nil, false, nil // delta entry: general path
		}
		typ, ok := codeType[tc]
		if !ok {
			return nil, false, nil // unknown type: general path reports it
		}
		inflated, consumed, err := inf.inflate(pack, offset, trailerStart, size)
		if err != nil {
			return nil, true, err
		}
		offset += consumed
		oid := hashObject(typ, inflated)
		if _, ok := objects[oid]; !ok {
			objects[oid] = gitObject{typ: typ, data: inflated}
		}
	}
	if offset != trailerStart {
		return nil, true, errors.New("pack has trailing bytes before its trailer")
	}
	return objects, true, nil
}

func readPackGeneral(pack []byte, lookup func(string) *gitObject) (map[string]gitObject, error) {
	if len(pack) < 32 {
		return nil, errors.New("pack too short")
	}
	if string(pack[0:4]) != "PACK" {
		return nil, errors.New("not a pack")
	}
	version := int(pack[4])<<24 | int(pack[5])<<16 | int(pack[6])<<8 | int(pack[7])
	if version != 2 && version != 3 {
		return nil, fmt.Errorf("unsupported pack version %d", version)
	}
	declaredCount := int(pack[8])<<24 | int(pack[9])<<16 | int(pack[10])<<8 | int(pack[11])
	trailerStart := len(pack) - 20
	raw := make([]*rawEntry, 0, declaredCount)
	byOffset := make(map[int]*rawEntry, declaredCount)
	var inf inflater
	offset := 12
	for i := 0; i < declaredCount; i++ {
		entryOffset := offset
		size, next, first, err := readVarintSize(pack, offset)
		if err != nil {
			return nil, err
		}
		offset = next
		tc := (first >> 4) & 0x07
		entry := &rawEntry{offset: entryOffset, typeCode: tc, size: size}
		if tc == 6 {
			baseOff, next2, err := readOfsDeltaBase(pack, offset)
			if err != nil {
				return nil, err
			}
			offset = next2
			entry.baseOff = entryOffset - baseOff
			entry.hasBase = true
		} else if tc == 7 {
			if offset+20 > len(pack) {
				return nil, errors.New("pack truncated")
			}
			entry.baseOID = hex.EncodeToString(pack[offset : offset+20])
			entry.hasBase = true
			offset += 20
		} else if _, ok := codeType[tc]; !ok {
			return nil, fmt.Errorf("unknown pack type code %d", tc)
		}
		inflated, consumed, err := inf.inflate(pack, offset, trailerStart, size)
		if err != nil {
			return nil, err
		}
		if len(inflated) != size {
			return nil, errors.New("pack entry size mismatch")
		}
		entry.data = inflated
		offset += consumed
		raw = append(raw, entry)
		byOffset[entryOffset] = entry
	}
	if offset != trailerStart {
		return nil, errors.New("pack has trailing bytes before its trailer")
	}

	objects := make(map[string]gitObject, len(raw))
	resolvedByOID := make(map[string]*rawEntry)
	resolving := make(map[int]bool)
	var resolve func(e *rawEntry) (*resolvedObj, error)
	resolve = func(e *rawEntry) (*resolvedObj, error) {
		if e.resolved != nil {
			return e.resolved, nil
		}
		if resolving[e.offset] {
			return nil, errors.New("delta cycle")
		}
		resolving[e.offset] = true
		defer delete(resolving, e.offset)
		var typ gitObjectType
		var data []byte
		if e.typeCode == 6 {
			baseEntry, ok := byOffset[e.baseOff]
			if !ok {
				return nil, errors.New("ofs-delta base missing")
			}
			base, err := resolve(baseEntry)
			if err != nil {
				return nil, err
			}
			typ = base.typ
			data, err = applyDelta(base.data, e.data)
			if err != nil {
				return nil, err
			}
		} else if e.typeCode == 7 {
			if baseEntry, ok := resolvedByOID[e.baseOID]; ok {
				base, err := resolve(baseEntry)
				if err != nil {
					return nil, err
				}
				typ = base.typ
				data, err = applyDelta(base.data, e.data)
				if err != nil {
					return nil, err
				}
			} else {
				external := lookup(e.baseOID)
				if external == nil {
					return nil, fmt.Errorf("ref-delta base %s missing", e.baseOID)
				}
				typ = external.typ
				var err error
				data, err = applyDelta(external.data, e.data)
				if err != nil {
					return nil, err
				}
			}
		} else {
			typ = codeType[e.typeCode]
			data = e.data
		}
		oid := hashObject(typ, data)
		out := &resolvedObj{typ: typ, data: data, oid: oid}
		e.resolved = out
		resolvedByOID[oid] = e
		return out, nil
	}
	for _, e := range raw {
		resolved, err := resolve(e)
		if err != nil {
			return nil, err
		}
		if _, ok := objects[resolved.oid]; !ok {
			objects[resolved.oid] = gitObject{typ: resolved.typ, data: resolved.data}
		}
	}
	return objects, nil
}
