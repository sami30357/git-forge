// A standalone GitHub-compatible git forge: git smart HTTP (protocol v2 + v0), REST v3,
// GraphQL v4 and MCP, all from one process over one in-memory store. Stdlib only:
// node:http, node:crypto, node:zlib. Deterministic: no wall clock or Math.random in any
// stored state or response (a counter drives timestamps and request ids).
//
// Entry point: createApp() returns the http.Server the lifecycle (main.ts) binds.

import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import type { IncomingMessage, Server, ServerResponse } from 'node:http';
import { deflateSync, inflateSync } from 'node:zlib';

// -------------------------------------------------------------------------------------------
// constants
// -------------------------------------------------------------------------------------------

export const VIEWER_LOGIN = 'adaiversen';
const VIEWER_ID = 1000;
const VIEWER_NAME = 'ada iversen';
const VIEWER_EMAIL = 'adaiversen@example.invalid';
const ZERO = '0'.repeat(40);
const EMPTY_TREE = '4b825dc642cb6eb9a060e54bf8d69288fbee4904';
const BASE_TS = 1735689600; // 2025-01-01T00:00:00Z, the scenario epoch
const IDENT = 'ada iversen <adaiversen@example.invalid>';
const SERVER_NAME = 'forge';
const SERVER_VERSION = '1.0.0';

type GitObjectType = 'commit' | 'tree' | 'blob' | 'tag';
interface GitObject {
  type: GitObjectType;
  data: Buffer;
}

const TYPE_CODE: Record<GitObjectType, number> = { commit: 1, tree: 2, blob: 3, tag: 4 };
const CODE_TYPE: Record<number, GitObjectType> = { 1: 'commit', 2: 'tree', 3: 'blob', 4: 'tag' };
const HEX40 = /^[0-9a-f]{40}$/;
const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/;

// -------------------------------------------------------------------------------------------
// small helpers
// -------------------------------------------------------------------------------------------

function sha1(data: Buffer): string {
  return createHash('sha1').update(data).digest('hex');
}

function hashObject(type: GitObjectType, data: Buffer): string {
  const h = createHash('sha1');
  h.update(Buffer.from(`${type} ${data.length}\0`, 'utf8'));
  h.update(data);
  return h.digest('hex');
}

function isOid(value: string): boolean {
  return HEX40.test(value);
}

function b64encode(text: string): string {
  return Buffer.from(text, 'utf8').toString('base64');
}

function b64decode(text: string): Buffer {
  return Buffer.from(text.replace(/\s+/g, ''), 'base64');
}

function isoOf(ts: number): string {
  return new Date(ts * 1000).toISOString();
}

/** Multiset line difference: [added lines, removed lines]. */
function lineDiff(oldText: string, newText: string): [number, number] {
  const count = (text: string): Map<string, number> => {
    const m = new Map<string, number>();
    if (text === '') return m;
    for (const line of text.split('\n')) m.set(line, (m.get(line) ?? 0) + 1);
    return m;
  };
  const a = count(oldText);
  const b = count(newText);
  let added = 0;
  let removed = 0;
  for (const [line, n] of b) added += Math.max(0, n - (a.get(line) ?? 0));
  for (const [line, n] of a) removed += Math.max(0, n - (b.get(line) ?? 0));
  return [added, removed];
}

// -------------------------------------------------------------------------------------------
// pkt-lines
// -------------------------------------------------------------------------------------------

const PKT_MAX = 65520;

function pktLine(payload: string | Buffer): Buffer {
  const data = typeof payload === 'string' ? Buffer.from(payload, 'utf8') : payload;
  if (data.length + 4 > PKT_MAX) throw new Error('pkt-line payload too long');
  const header = Buffer.allocUnsafe(4);
  header.write((data.length + 4).toString(16).padStart(4, '0'), 0, 4, 'ascii');
  return Buffer.concat([header, data]);
}

function pktFlush(): Buffer {
  return Buffer.from('0000', 'ascii');
}

function pktDelim(): Buffer {
  return Buffer.from('0001', 'ascii');
}

function pktLines(items: ReadonlyArray<string | Buffer | 'flush' | 'delim'>): Buffer {
  const parts: Buffer[] = [];
  for (const item of items) {
    if (item === 'flush') parts.push(pktFlush());
    else if (item === 'delim') parts.push(pktDelim());
    else parts.push(pktLine(item));
  }
  return Buffer.concat(parts);
}

type PktPacket = { kind: 'flush' } | { kind: 'delim' } | { kind: 'data'; payload: Buffer };

/**
 * Decode pkt-lines from the head of a buffer. Stops at the first flush (returning the bytes
 * after it as `rest`) or at the end of the buffer.
 */
function decodePktLines(buf: Buffer): { packets: PktPacket[]; rest: Buffer } {
  const packets: PktPacket[] = [];
  let offset = 0;
  while (offset + 4 <= buf.length) {
    const header = buf.subarray(offset, offset + 4).toString('ascii');
    if (!/^[0-9a-fA-F]{4}$/.test(header)) break;
    const length = Number.parseInt(header, 16);
    if (length === 0) {
      packets.push({ kind: 'flush' });
      return { packets, rest: buf.subarray(offset + 4) };
    }
    if (length === 1) {
      packets.push({ kind: 'delim' });
      offset += 4;
      continue;
    }
    if (length === 2 || length === 3 || length > PKT_MAX || offset + length > buf.length) break;
    packets.push({ kind: 'data', payload: buf.subarray(offset + 4, offset + length) });
    offset += length;
  }
  return { packets, rest: buf.subarray(offset) };
}

function packetText(packet: PktPacket): string {
  return packet.kind === 'data' ? packet.payload.toString('utf8') : '';
}

// -------------------------------------------------------------------------------------------
// git object model
// -------------------------------------------------------------------------------------------

interface TreeEntry {
  mode: string;
  name: string;
  oid: string;
}

function parseTree(data: Buffer): TreeEntry[] {
  const entries: TreeEntry[] = [];
  let offset = 0;
  while (offset < data.length) {
    const space = data.indexOf(0x20, offset);
    const nul = data.indexOf(0x00, space + 1);
    if (space < 0 || nul < 0 || nul + 21 > data.length) throw new Error('bad tree object');
    entries.push({
      mode: data.subarray(offset, space).toString('ascii'),
      name: data.subarray(space + 1, nul).toString('utf8'),
      oid: data.subarray(nul + 1, nul + 21).toString('hex'),
    });
    offset = nul + 21;
  }
  return entries;
}

function compareTreeNames(a: { name: string; mode: string }, b: { name: string; mode: string }): number {
  const left = Buffer.from(a.mode === '40000' ? `${a.name}/` : a.name, 'utf8');
  const right = Buffer.from(b.mode === '40000' ? `${b.name}/` : b.name, 'utf8');
  return Buffer.compare(left, right);
}

function buildTree(entries: readonly TreeEntry[]): Buffer {
  const sorted = [...entries].sort(compareTreeNames);
  const parts: Buffer[] = [];
  for (const entry of sorted) {
    parts.push(Buffer.from(`${entry.mode} ${entry.name}\0`, 'utf8'));
    parts.push(Buffer.from(entry.oid, 'hex'));
  }
  return Buffer.concat(parts);
}

interface CommitView {
  tree: string;
  parents: string[];
  author: string;
  committer: string;
  message: string;
}

function parseCommit(data: Buffer): CommitView {
  const blank = data.indexOf('\n\n');
  const headerEnd = blank < 0 ? data.length : blank;
  const message = blank < 0 ? '' : data.subarray(blank + 2).toString('utf8');
  let tree = '';
  const parents: string[] = [];
  let author = '';
  let committer = '';
  for (const line of data.subarray(0, headerEnd).toString('utf8').split('\n')) {
    if (!line) continue;
    const space = line.indexOf(' ');
    const key = space < 0 ? line : line.slice(0, space);
    const value = space < 0 ? '' : line.slice(space + 1);
    if (key === 'tree') tree = value;
    else if (key === 'parent') parents.push(value);
    else if (key === 'author') author = value;
    else if (key === 'committer') committer = value;
  }
  return { tree, parents, author, committer, message };
}

function buildCommit(tree: string, parents: readonly string[], message: string, timestamp: number): Buffer {
  const lines = [`tree ${tree}`];
  for (const parent of parents) lines.push(`parent ${parent}`);
  lines.push(`author ${IDENT} ${timestamp} +0000`, `committer ${IDENT} ${timestamp} +0000`, '');
  return Buffer.from(`${lines.join('\n')}\n${message}`, 'utf8');
}

function buildTag(object: string, type: GitObjectType, tag: string, message: string, timestamp: number): Buffer {
  return Buffer.from(
    `object ${object}\ntype ${type}\ntag ${tag}\ntagger ${IDENT} ${timestamp} +0000\n\n${message}`,
    'utf8',
  );
}

function parseTag(data: Buffer): { object: string; type: string; tag: string; message: string } {
  const blank = data.indexOf('\n\n');
  const headerEnd = blank < 0 ? data.length : blank;
  const headers = new Map<string, string>();
  for (const line of data.subarray(0, headerEnd).toString('utf8').split('\n')) {
    if (!line) continue;
    const space = line.indexOf(' ');
    if (space < 0) continue;
    headers.set(line.slice(0, space), line.slice(space + 1));
  }
  return {
    object: headers.get('object') ?? '',
    type: headers.get('type') ?? '',
    tag: headers.get('tag') ?? '',
    message: blank < 0 ? '' : data.subarray(blank + 2).toString('utf8'),
  };
}

/** Every object reachable from roots (commit -> tree + parents, tag -> target, tree -> entries). */
function collectReachable(
  roots: Iterable<string>,
  lookup: (oid: string) => GitObject | undefined,
  allowMissing = false,
): Set<string> {
  const seen = new Set<string>();
  const queue: string[] = [];
  for (const root of roots) if (isOid(root)) queue.push(root);
  while (queue.length) {
    const oid = queue.pop()!;
    if (seen.has(oid)) continue;
    const object = lookup(oid);
    if (!object) {
      if (allowMissing) continue;
      throw new Error(`object ${oid} is missing`);
    }
    seen.add(oid);
    if (object.type === 'commit') {
      const commit = parseCommit(object.data);
      queue.push(commit.tree);
      for (const parent of commit.parents) queue.push(parent);
    } else if (object.type === 'tag') {
      queue.push(parseTag(object.data).object);
    } else if (object.type === 'tree') {
      for (const entry of parseTree(object.data)) {
        if (entry.mode !== '160000') queue.push(entry.oid);
      }
    }
  }
  return seen;
}

// -------------------------------------------------------------------------------------------
// packfiles (plain objects only; deltas resolved on read)
// -------------------------------------------------------------------------------------------

function writePackObjectHeader(type: GitObjectType, size: number): Buffer {
  const code = TYPE_CODE[type];
  const bytes: number[] = [];
  let remaining = size;
  let first = (code << 4) | (remaining & 0x0f);
  remaining = Math.floor(remaining / 16);
  while (remaining > 0) {
    bytes.push(first | 0x80);
    first = remaining & 0x7f;
    remaining = Math.floor(remaining / 128);
  }
  bytes.push(first);
  return Buffer.from(bytes);
}

function writePack(objects: readonly GitObject[]): Buffer {
  const parts: Buffer[] = [];
  const header = Buffer.allocUnsafe(12);
  header.write('PACK', 0, 4, 'ascii');
  header.writeUInt32BE(2, 4);
  header.writeUInt32BE(objects.length, 8);
  parts.push(header);
  for (const object of objects) {
    parts.push(writePackObjectHeader(object.type, object.data.length));
    parts.push(deflateSync(object.data));
  }
  const body = Buffer.concat(parts);
  return Buffer.concat([body, createHash('sha1').update(body).digest()]);
}

function readVarintSize(buffer: Buffer, start: number): { size: number; next: number; first: number } {
  let byte = buffer[start];
  const first = byte;
  let size = byte & 0x0f;
  let shift = 4;
  let offset = start + 1;
  while (byte & 0x80) {
    byte = buffer[offset++];
    size += (byte & 0x7f) * 2 ** shift;
    shift += 7;
  }
  return { size, next: offset, first };
}

function readOfsDeltaBase(buffer: Buffer, start: number): { baseOffset: number; next: number } {
  let byte = buffer[start];
  let offset = start + 1;
  let value = byte & 0x7f;
  while (byte & 0x80) {
    byte = buffer[offset++];
    value = (value + 1) * 128 + (byte & 0x7f);
  }
  return { baseOffset: value, next: offset };
}

function applyDelta(base: Buffer, delta: Buffer): Buffer {
  let offset = 0;
  const readSize = (): number => {
    let value = 0;
    let shift = 0;
    for (;;) {
      const byte = delta[offset++];
      value += (byte & 0x7f) * 2 ** shift;
      shift += 7;
      if (!(byte & 0x80)) break;
    }
    return value;
  };
  const sourceSize = readSize();
  const targetSize = readSize();
  if (sourceSize !== base.length) throw new Error('delta base size mismatch');
  const out = Buffer.allocUnsafe(targetSize);
  let written = 0;
  while (offset < delta.length) {
    const op = delta[offset++];
    if (op & 0x80) {
      let copyOffset = 0;
      let copySize = 0;
      let shift = 0;
      for (let bit = 0; bit < 4; bit += 1) {
        if (op & (1 << bit)) copyOffset += delta[offset++] * 2 ** shift;
        shift += 8;
      }
      shift = 0;
      for (let bit = 4; bit < 7; bit += 1) {
        if (op & (1 << bit)) copySize += delta[offset++] * 2 ** shift;
        shift += 8;
      }
      if (copySize === 0) copySize = 0x10000;
      base.copy(out, written, copyOffset, copyOffset + copySize);
      written += copySize;
    } else if (op !== 0) {
      const literal = op & 0x7f;
      delta.copy(out, written, offset, offset + literal);
      offset += literal;
      written += literal;
    } else {
      throw new Error('delta opcode 0');
    }
  }
  if (written !== targetSize) throw new Error('delta size mismatch');
  return out;
}

/** Read a packfile; resolves OFS/REF deltas (REF bases may come from the store). */
function readPack(pack: Buffer, lookup: (oid: string) => GitObject | undefined): Map<string, GitObject> {
  if (pack.length < 32) throw new Error('pack too short');
  if (pack.subarray(0, 4).toString('ascii') !== 'PACK') throw new Error('not a pack');
  const version = pack.readUInt32BE(4);
  if (version !== 2 && version !== 3) throw new Error(`unsupported pack version ${version}`);
  const declaredCount = pack.readUInt32BE(8);
  const trailerStart = pack.length - 20;
  interface RawEntry {
    offset: number;
    typeCode: number;
    size: number;
    data: Buffer;
    baseOffset?: number;
    baseOid?: string;
  }
  const raw: RawEntry[] = [];
  const byOffset = new Map<number, RawEntry>();
  let offset = 12;
  for (let index = 0; index < declaredCount; index += 1) {
    const entryOffset = offset;
    const header = readVarintSize(pack, offset);
    offset = header.next;
    const typeCode = (header.first >> 4) & 0x07;
    const entry: RawEntry = { offset: entryOffset, typeCode, size: header.size, data: Buffer.alloc(0) };
    if (typeCode === 6) {
      const base = readOfsDeltaBase(pack, offset);
      offset = base.next;
      entry.baseOffset = entryOffset - base.baseOffset;
    } else if (typeCode === 7) {
      entry.baseOid = pack.subarray(offset, offset + 20).toString('hex');
      offset += 20;
    } else if (!CODE_TYPE[typeCode]) {
      throw new Error(`unknown pack type code ${typeCode}`);
    }
    const inflated = inflateSync(pack.subarray(offset, trailerStart), { info: true }) as unknown as {
      buffer: Buffer;
      engine: { bytesWritten: number };
    };
    if (inflated.buffer.length !== header.size) throw new Error('pack entry size mismatch');
    entry.data = inflated.buffer;
    offset += inflated.engine.bytesWritten;
    raw.push(entry);
    byOffset.set(entryOffset, entry);
  }
  if (offset !== trailerStart) throw new Error('pack has trailing bytes before its trailer');

  const objects = new Map<string, GitObject>();
  const resolvedByOid = new Map<string, RawEntry>();
  const resolving = new Set<number>();
  const resolve = (entry: RawEntry): { type: GitObjectType; data: Buffer; oid: string } => {
    const cached = (entry as unknown as { resolved?: { type: GitObjectType; data: Buffer; oid: string } }).resolved;
    if (cached) return cached;
    if (resolving.has(entry.offset)) throw new Error('delta cycle');
    resolving.add(entry.offset);
    let type: GitObjectType;
    let data: Buffer;
    if (entry.typeCode === 6) {
      const baseEntry = byOffset.get(entry.baseOffset!);
      if (!baseEntry) throw new Error('ofs-delta base missing');
      const base = resolve(baseEntry);
      type = base.type;
      data = applyDelta(base.data, entry.data);
    } else if (entry.typeCode === 7) {
      const wanted = entry.baseOid!;
      const baseEntry = resolvedByOid.get(wanted);
      if (baseEntry) {
        const base = resolve(baseEntry);
        type = base.type;
        data = applyDelta(base.data, entry.data);
      } else {
        const external = lookup(wanted);
        if (!external) throw new Error(`ref-delta base ${wanted} missing`);
        type = external.type;
        data = applyDelta(external.data, entry.data);
      }
    } else {
      type = CODE_TYPE[entry.typeCode];
      data = entry.data;
    }
    const oid = hashObject(type, data);
    const out = { type, data, oid };
    (entry as unknown as { resolved: unknown }).resolved = out;
    resolvedByOid.set(oid, entry);
    resolving.delete(entry.offset);
    return out;
  };
  for (const entry of raw) {
    const resolved = resolve(entry);
    if (!objects.has(resolved.oid)) objects.set(resolved.oid, { type: resolved.type, data: resolved.data });
  }
  return objects;
}

// -------------------------------------------------------------------------------------------
// tree path operations (contents API + merge base/diff)
// -------------------------------------------------------------------------------------------

type Lookup = (oid: string) => GitObject | undefined;

/** Flatten a tree to path -> {oid, mode} (files only; dirs recursed). */
function flattenTree(rootOid: string | null, lookup: Lookup): Map<string, { oid: string; mode: string }> {
  const out = new Map<string, { oid: string; mode: string }>();
  if (rootOid === null) return out;
  const walk = (treeOid: string, prefix: string): void => {
    const obj = lookup(treeOid);
    if (!obj || obj.type !== 'tree') return;
    for (const entry of parseTree(obj.data)) {
      const path = prefix === '' ? entry.name : `${prefix}/${entry.name}`;
      if (entry.mode === '40000') walk(entry.oid, path);
      else out.set(path, { oid: entry.oid, mode: entry.mode });
    }
  };
  walk(rootOid, '');
  return out;
}

/**
 * Set (or replace) the blob at `segments` in the tree rooted at `rootOid` (null = empty tree).
 * Returns the new root tree oid; new tree objects are written through `put`.
 */
function setPath(
  rootOid: string | null,
  segments: readonly string[],
  blobOid: string,
  lookup: Lookup,
  put: (type: GitObjectType, data: Buffer) => string,
): string {
  const rec = (treeOid: string | null, depth: number): string => {
    const entries: TreeEntry[] = treeOid === null ? [] : parseTree(lookup(treeOid)?.data ?? Buffer.alloc(0));
    const name = segments[depth];
    if (depth === segments.length - 1) {
      const idx = entries.findIndex((e) => e.name === name);
      if (idx >= 0) entries[idx] = { mode: '100644', name, oid: blobOid };
      else entries.push({ mode: '100644', name, oid: blobOid });
      return put('tree', buildTree(entries));
    }
    const idx = entries.findIndex((e) => e.name === name);
    const childOid = idx >= 0 && entries[idx].mode === '40000' ? entries[idx].oid : null;
    const newChild = rec(childOid, depth + 1);
    if (idx >= 0) entries[idx] = { mode: '40000', name, oid: newChild };
    else entries.push({ mode: '40000', name, oid: newChild });
    return put('tree', buildTree(entries));
  };
  return rec(rootOid, 0);
}

/** Delete the path at `segments` from the tree; returns the new root tree oid. */
function deletePath(
  rootOid: string | null,
  segments: readonly string[],
  lookup: Lookup,
  put: (type: GitObjectType, data: Buffer) => string,
): string {
  if (rootOid === null) return EMPTY_TREE;
  const rec = (treeOid: string, depth: number): string => {
    const entries: TreeEntry[] = parseTree(lookup(treeOid)?.data ?? Buffer.alloc(0));
    const name = segments[depth];
    const idx = entries.findIndex((e) => e.name === name);
    if (idx < 0) return treeOid;
    if (depth === segments.length - 1) {
      entries.splice(idx, 1);
      return put('tree', buildTree(entries));
    }
    if (entries[idx].mode !== '40000') return treeOid;
    const newChild = rec(entries[idx].oid, depth + 1);
    if (newChild === entries[idx].oid) return treeOid;
    const childEntries = parseTree(lookup(newChild)?.data ?? Buffer.alloc(0));
    if (childEntries.length === 0) entries.splice(idx, 1);
    else entries[idx] = { mode: '40000', name, oid: newChild };
    return put('tree', buildTree(entries));
  };
  return rec(rootOid, 0);
}

/** The blob oid at `segments`, or null when the path is missing or a directory. */
function getPath(rootOid: string | null, segments: readonly string[], lookup: Lookup): string | null {
  if (rootOid === null) return null;
  let treeOid = rootOid;
  for (let depth = 0; depth < segments.length; depth += 1) {
    const obj = lookup(treeOid);
    if (!obj || obj.type !== 'tree') return null;
    const entry = parseTree(obj.data).find((e) => e.name === segments[depth]);
    if (!entry) return null;
    if (depth === segments.length - 1) return entry.mode === '40000' ? null : entry.oid;
    if (entry.mode !== '40000') return null;
    treeOid = entry.oid;
  }
  return null;
}

/** The entries of the directory at `segments` (empty segments = the root), or null. */
function listDir(rootOid: string | null, segments: readonly string[], lookup: Lookup): TreeEntry[] | null {
  if (rootOid === null) return segments.length === 0 ? [] : null;
  let treeOid = rootOid;
  for (let depth = 0; depth < segments.length; depth += 1) {
    const obj = lookup(treeOid);
    if (!obj || obj.type !== 'tree') return null;
    const entry = parseTree(obj.data).find((e) => e.name === segments[depth]);
    if (!entry || entry.mode !== '40000') return null;
    treeOid = entry.oid;
  }
  const obj = lookup(treeOid);
  if (!obj || obj.type !== 'tree') return null;
  return parseTree(obj.data);
}

/** True when `ancestor` is reachable from `tip` (following parents). */
function isDescendant(tip: string, ancestor: string, lookup: Lookup): boolean {
  if (tip === ancestor) return true;
  const seen = new Set<string>();
  const queue = [tip];
  while (queue.length) {
    const oid = queue.pop()!;
    if (seen.has(oid)) continue;
    seen.add(oid);
    const obj = lookup(oid);
    if (!obj || obj.type !== 'commit') continue;
    for (const parent of parseCommit(obj.data).parents) {
      if (parent === ancestor) return true;
      queue.push(parent);
    }
  }
  return false;
}

/** The first common ancestor of two commits (walking parents), or null. */
function mergeBase(a: string, b: string, lookup: Lookup): string | null {
  const ancestors = new Set<string>();
  const queueA = [a];
  while (queueA.length) {
    const oid = queueA.pop()!;
    if (ancestors.has(oid)) continue;
    ancestors.add(oid);
    const obj = lookup(oid);
    if (obj?.type === 'commit') for (const p of parseCommit(obj.data).parents) queueA.push(p);
  }
  const queueB = [b];
  const seenB = new Set<string>();
  while (queueB.length) {
    const oid = queueB.pop()!;
    if (seenB.has(oid)) continue;
    seenB.add(oid);
    if (ancestors.has(oid)) return oid;
    const obj = lookup(oid);
    if (obj?.type === 'commit') for (const p of parseCommit(obj.data).parents) queueB.push(p);
  }
  return null;
}

interface FileChange {
  filename: string;
  status: 'added' | 'modified' | 'removed';
  additions: number;
  deletions: number;
  changes: number;
  oldOid: string | null;
  newOid: string | null;
}

/** Diff base tip tree -> head tip tree, with line counts from the blobs. */
function diffTrees(baseTip: string | null, headTip: string | null, lookup: Lookup): FileChange[] {
  const baseTree = baseTip === null ? null : parseCommit(lookup(baseTip)?.data ?? Buffer.alloc(0)).tree;
  const headTree = headTip === null ? null : parseCommit(lookup(headTip)?.data ?? Buffer.alloc(0)).tree;
  const baseFlat = flattenTree(baseTree, lookup);
  const headFlat = flattenTree(headTree, lookup);
  const paths = new Set([...baseFlat.keys(), ...headFlat.keys()]);
  const out: FileChange[] = [];
  for (const path of [...paths].sort()) {
    const oldEntry = baseFlat.get(path);
    const newEntry = headFlat.get(path);
    if (oldEntry && newEntry && oldEntry.oid === newEntry.oid) continue;
    const oldText = oldEntry ? (lookup(oldEntry.oid)?.data.toString('utf8') ?? '') : '';
    const newText = newEntry ? (lookup(newEntry.oid)?.data.toString('utf8') ?? '') : '';
    const [additions, deletions] = lineDiff(oldText, newText);
    out.push({
      filename: path,
      status: !oldEntry ? 'added' : !newEntry ? 'removed' : 'modified',
      additions,
      deletions,
      changes: additions + deletions,
      oldOid: oldEntry?.oid ?? null,
      newOid: newEntry?.oid ?? null,
    });
  }
  return out;
}

/** True when merging head into base would conflict (three-way rule). */
function mergeConflicts(baseTip: string, headTip: string, lookup: Lookup): boolean {
  const base = mergeBase(baseTip, headTip, lookup);
  if (base === null) return false;
  const baseTree = parseCommit(lookup(base)?.data ?? Buffer.alloc(0)).tree;
  const baseTipTree = parseCommit(lookup(baseTip)?.data ?? Buffer.alloc(0)).tree;
  const headTipTree = parseCommit(lookup(headTip)?.data ?? Buffer.alloc(0)).tree;
  const baseFlat = flattenTree(baseTree, lookup);
  const baseTipFlat = flattenTree(baseTipTree, lookup);
  const headTipFlat = flattenTree(headTipTree, lookup);
  const paths = new Set([...baseFlat.keys(), ...baseTipFlat.keys(), ...headTipFlat.keys()]);
  for (const path of paths) {
    const m = baseFlat.get(path)?.oid ?? null;
    const b = baseTipFlat.get(path)?.oid ?? null;
    const h = headTipFlat.get(path)?.oid ?? null;
    if (b !== m && h !== m && b !== h) return true;
  }
  return false;
}

// -------------------------------------------------------------------------------------------
// store
// -------------------------------------------------------------------------------------------

interface Label {
  id: number;
  name: string;
  color: string;
}

interface Comment {
  id: number;
  body: string;
  createdAt: string;
  updatedAt: string;
}

interface Issue {
  number: number;
  id: number;
  title: string;
  body: string;
  state: 'open' | 'closed';
  labels: string[];
  createdAt: string;
  updatedAt: string;
  closedAt: string | null;
  comments: Comment[];
}

interface PullRequest {
  number: number;
  id: number;
  title: string;
  body: string;
  state: 'open' | 'closed';
  headRef: string;
  baseRef: string;
  headSha: string;
  baseSha: string;
  merged: boolean;
  mergedAt: string | null;
  mergeCommitSha: string | null;
  createdAt: string;
  updatedAt: string;
  closedAt: string | null;
}

interface Repo {
  id: number;
  name: string;
  owner: string;
  isPrivate: boolean;
  description: string;
  defaultBranch: string;
  createdAt: string;
  updatedAt: string;
  pushedAt: string;
  refs: Map<string, string>; // ref name -> tip oid
  issues: Map<number, Issue>;
  issueCounter: number;
  pulls: Map<number, PullRequest>;
  pullCounter: number;
  labels: Map<string, Label>;
  labelCounter: number;
  commentCounter: number;
}

class Store {
  objects = new Map<string, GitObject>();
  repos = new Map<string, Repo>();
  repoCounter = 0;
  commentIdCounter = 0;
  tsCounter = 0;
  reqCounter = 0;

  lookup = (oid: string): GitObject | undefined => this.objects.get(oid);

  putObject(type: GitObjectType, data: Buffer): string {
    const oid = hashObject(type, data);
    if (!this.objects.has(oid)) this.objects.set(oid, { type, data });
    return oid;
  }

  nextTs(): number {
    this.tsCounter += 1;
    return BASE_TS + this.tsCounter;
  }

  nextReqId(): string {
    this.reqCounter += 1;
    return `req-${this.reqCounter.toString(16).padStart(8, '0')}`;
  }

  repoKey(owner: string, name: string): string {
    return `${owner}/${name}`;
  }

  getRepo(owner: string, name: string): Repo | undefined {
    return this.repos.get(this.repoKey(owner, name));
  }

  getRepoById(id: number): Repo | undefined {
    for (const repo of this.repos.values()) if (repo.id === id) return repo;
    return undefined;
  }

  createRepo(owner: string, name: string, isPrivate: boolean, description: string): Repo {
    this.repoCounter += 1;
    const ts = this.nextTs();
    const repo: Repo = {
      id: this.repoCounter,
      name,
      owner,
      isPrivate,
      description,
      defaultBranch: 'main',
      createdAt: isoOf(ts),
      updatedAt: isoOf(ts),
      pushedAt: isoOf(ts),
      refs: new Map(),
      issues: new Map(),
      issueCounter: 0,
      pulls: new Map(),
      pullCounter: 0,
      labels: new Map(),
      labelCounter: 0,
      commentCounter: 0,
    };
    this.repos.set(this.repoKey(owner, name), repo);
    return repo;
  }

  deleteRepo(owner: string, name: string): void {
    this.repos.delete(this.repoKey(owner, name));
  }

  /** Create a commit on a branch (or an unborn repo) with the given tree and parents. */
  commitOn(repo: Repo, branch: string, treeOid: string, parents: string[], message: string): string {
    const ts = this.nextTs();
    const oid = this.putObject('commit', buildCommit(treeOid, parents, message, ts));
    repo.refs.set(`refs/heads/${branch}`, oid);
    repo.updatedAt = isoOf(ts);
    repo.pushedAt = isoOf(ts);
    return oid;
  }

  createIssue(repo: Repo, title: string, body: string, labels: string[]): Issue {
    repo.issueCounter += 1;
    const ts = this.nextTs();
    const issue: Issue = {
      number: repo.issueCounter,
      id: repo.issueCounter,
      title,
      body,
      state: 'open',
      labels: [...labels],
      createdAt: isoOf(ts),
      updatedAt: isoOf(ts),
      closedAt: null,
      comments: [],
    };
    repo.issues.set(issue.number, issue);
    return issue;
  }

  createPullRequest(repo: Repo, title: string, body: string, headRef: string, baseRef: string): PullRequest | null {
    const headSha = repo.refs.get(`refs/heads/${headRef}`);
    const baseSha = repo.refs.get(`refs/heads/${baseRef}`);
    if (!headSha || !baseSha) return null;
    repo.pullCounter += 1;
    const ts = this.nextTs();
    const pr: PullRequest = {
      number: repo.pullCounter,
      id: repo.pullCounter,
      title,
      body,
      state: 'open',
      headRef,
      baseRef,
      headSha,
      baseSha,
      merged: false,
      mergedAt: null,
      mergeCommitSha: null,
      createdAt: isoOf(ts),
      updatedAt: isoOf(ts),
      closedAt: null,
    };
    repo.pulls.set(pr.number, pr);
    return pr;
  }

  addComment(repo: Repo, issue: Issue, body: string): Comment {
    this.commentIdCounter += 1;
    const ts = this.nextTs();
    const comment: Comment = {
      id: this.commentIdCounter,
      body,
      createdAt: isoOf(ts),
      updatedAt: isoOf(ts),
    };
    issue.comments.push(comment);
    return comment;
  }
}

// -------------------------------------------------------------------------------------------
// HTTP plumbing
// -------------------------------------------------------------------------------------------

interface Ctx {
  store: Store;
  req: IncomingMessage;
  res: ServerResponse;
  method: string;
  path: string;
  query: URLSearchParams;
  body: Buffer;
  token: string;
  origin: string;
}

function gid(...parts: Array<string | number>): string {
  return b64encode(parts.join(':'));
}

function userJson(): Record<string, unknown> {
  return {
    login: VIEWER_LOGIN,
    id: VIEWER_ID,
    node_id: gid('User', VIEWER_ID),
    name: VIEWER_NAME,
    email: VIEWER_EMAIL,
    type: 'User',
    site_admin: false,
    avatar_url: `https://example.invalid/avatar/${VIEWER_LOGIN}`,
    html_url: `https://example.invalid/${VIEWER_LOGIN}`,
    url: `/api/v3/users/${VIEWER_LOGIN}`,
  };
}

function repoJson(repo: Repo): Record<string, unknown> {
  return {
    id: repo.id,
    node_id: gid('Repository', repo.id),
    name: repo.name,
    full_name: `${repo.owner}/${repo.name}`,
    owner: { login: repo.owner, id: VIEWER_ID, type: 'User' },
    private: repo.isPrivate,
    description: repo.description,
    default_branch: repo.defaultBranch,
    created_at: repo.createdAt,
    updated_at: repo.updatedAt,
    pushed_at: repo.pushedAt,
    html_url: `https://example.invalid/${repo.owner}/${repo.name}`,
    url: `/api/v3/repos/${repo.owner}/${repo.name}`,
    fork: false,
    has_issues: true,
    has_projects: true,
    has_wiki: true,
  };
}

function labelJson(label: Label): Record<string, unknown> {
  return { id: label.id, node_id: gid('Label', label.id), name: label.name, color: label.color, description: '' };
}

function issueJson(repo: Repo, issue: Issue): Record<string, unknown> {
  return {
    id: issue.id,
    node_id: gid('Issue', repo.id, issue.number),
    number: issue.number,
    title: issue.title,
    body: issue.body,
    state: issue.state,
    labels: issue.labels.map((name) => {
      const label = repo.labels.get(name);
      return label ? labelJson(label) : { id: 0, name, color: '000000', description: '' };
    }),
    user: { login: VIEWER_LOGIN, id: VIEWER_ID, type: 'User' },
    assignee: null,
    assignees: [],
    created_at: issue.createdAt,
    updated_at: issue.updatedAt,
    closed_at: issue.closedAt,
    comments: issue.comments.length,
    html_url: `https://example.invalid/${repo.owner}/${repo.name}/issues/${issue.number}`,
    url: `/api/v3/repos/${repo.owner}/${repo.name}/issues/${issue.number}`,
  };
}

function commentJson(repo: Repo, issue: Issue, comment: Comment): Record<string, unknown> {
  return {
    id: comment.id,
    node_id: gid('IssueComment', comment.id),
    body: comment.body,
    user: { login: VIEWER_LOGIN, id: VIEWER_ID, type: 'User' },
    created_at: comment.createdAt,
    updated_at: comment.updatedAt,
    html_url: `https://example.invalid/${repo.owner}/${repo.name}/issues/${issue.number}#issuecomment-${comment.id}`,
    url: `/api/v3/repos/${repo.owner}/${repo.name}/issues/${issue.number}/comments/${comment.id}`,
  };
}

function prMergeable(store: Store, repo: Repo, pr: PullRequest): boolean {
  if (pr.state !== 'open') return true;
  const baseTip = repo.refs.get(`refs/heads/${pr.baseRef}`);
  const headTip = repo.refs.get(`refs/heads/${pr.headRef}`);
  if (!baseTip || !headTip) return false;
  return !mergeConflicts(baseTip, headTip, store.lookup);
}

function prJson(store: Store, repo: Repo, pr: PullRequest): Record<string, unknown> {
  const headTip = repo.refs.get(`refs/heads/${pr.headRef}`) ?? pr.headSha;
  const baseTip = repo.refs.get(`refs/heads/${pr.baseRef}`) ?? pr.baseSha;
  const mergeable = prMergeable(store, repo, pr);
  return {
    id: pr.id,
    node_id: gid('PullRequest', repo.id, pr.number),
    number: pr.number,
    state: pr.state,
    title: pr.title,
    body: pr.body,
    merged: pr.merged,
    merged_at: pr.mergedAt,
    merge_commit_sha: pr.mergeCommitSha,
    mergeable,
    mergeable_state: pr.state === 'open' ? (mergeable ? 'clean' : 'dirty') : 'unknown',
    head: { ref: pr.headRef, sha: headTip, label: `${repo.owner}:${pr.headRef}` },
    base: { ref: pr.baseRef, sha: baseTip, label: `${repo.owner}:${pr.baseRef}` },
    user: { login: VIEWER_LOGIN, id: VIEWER_ID, type: 'User' },
    created_at: pr.createdAt,
    updated_at: pr.updatedAt,
    closed_at: pr.closedAt,
    html_url: `https://example.invalid/${repo.owner}/${repo.name}/pull/${pr.number}`,
    url: `/api/v3/repos/${repo.owner}/${repo.name}/pulls/${pr.number}`,
  };
}

function sendJson(ctx: Ctx, status: number, value: unknown, extraHeaders?: Record<string, string>): void {
  const body = value === null ? '' : JSON.stringify(value);
  const headers: Record<string, string> = {
    'content-type': 'application/json',
    'x-github-request-id': ctx.store.nextReqId(),
  };
  if (extraHeaders) for (const [k, v] of Object.entries(extraHeaders)) headers[k] = v;
  ctx.res.writeHead(status, headers);
  ctx.res.end(body);
}

function sendErr(ctx: Ctx, status: number, message: string): void {
  sendJson(ctx, status, { message, documentation_url: 'https://example.invalid/docs' });
}

function authorized(ctx: Ctx): boolean {
  const header = ctx.req.headers.authorization ?? '';
  if (!header.startsWith('Bearer ')) return false;
  const presented = header.slice('Bearer '.length).trim();
  if (presented === '') return false;
  if (ctx.token === '') return true;
  return presented === ctx.token;
}

function parseJsonBody(ctx: Ctx): { ok: true; value: Record<string, unknown> } | { ok: false } {
  if (ctx.body.length === 0) return { ok: true, value: {} };
  try {
    const parsed: unknown = JSON.parse(ctx.body.toString('utf8'));
    if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return { ok: true, value: parsed as Record<string, unknown> };
    }
    return { ok: true, value: {} };
  } catch {
    return { ok: false };
  }
}

function pageParams(ctx: Ctx): { perPage: number; page: number } {
  const perPage = Math.max(1, Math.min(100, Number(ctx.query.get('per_page') ?? '30') || 30));
  const page = Math.max(1, Number(ctx.query.get('page') ?? '1') || 1);
  return { perPage, page };
}

function paginate<T>(ctx: Ctx, items: readonly T[], basePath: string): { page: T[]; link: string | null } {
  const { perPage, page } = pageParams(ctx);
  const start = (page - 1) * perPage;
  const slice = items.slice(start, start + perPage);
  let link: string | null = null;
  if (start + perPage < items.length) {
    const next = `${ctx.origin}${basePath}?per_page=${perPage}&page=${page + 1}`;
    link = `<${next}>; rel="next"`;
    if (page > 1) {
      const prev = `${ctx.origin}${basePath}?per_page=${perPage}&page=${page - 1}`;
      link += `, <${prev}>; rel="prev"`;
    }
  } else if (page > 1) {
    const prev = `${ctx.origin}${basePath}?per_page=${perPage}&page=${page - 1}`;
    link = `<${prev}>; rel="prev"`;
  }
  return { page: slice as T[], link };
}

function sortIssues(issues: Issue[], sort: string, direction: string): Issue[] {
  const sorted = [...issues];
  if (sort === 'created' || sort === '') {
    sorted.sort((a, b) => (direction === 'desc' ? b.number - a.number : a.number - b.number));
  }
  return sorted;
}

// -------------------------------------------------------------------------------------------
// REST v3
// -------------------------------------------------------------------------------------------

function repoOr404(ctx: Ctx, owner: string, name: string): Repo | null {
  if (owner !== VIEWER_LOGIN) return null;
  const repo = ctx.store.getRepo(owner, name);
  return repo ?? null;
}

function handleRest(ctx: Ctx): boolean {
  const seg = ctx.path.split('/').filter((s) => s !== '');
  // seg[0] === 'api', seg[1] === 'v3'
  if (seg.length < 2 || seg[0] !== 'api' || seg[1] !== 'v3') return false;
  const rest = seg.slice(2);
  const method = ctx.method;

  if (!authorized(ctx)) {
    sendErr(ctx, 401, 'Requires authentication');
    return true;
  }

  // GET /api/v3/
  if (rest.length === 0) {
    if (method !== 'GET') {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    sendJson(ctx, 200, {}, { 'x-oauth-scopes': 'repo, read:org', 'x-accepted-oauth-scopes': 'repo' });
    return true;
  }

  // GET /api/v3/meta
  if (rest.length === 1 && rest[0] === 'meta') {
    if (method !== 'GET') {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    sendJson(ctx, 200, { installed_version: SERVER_VERSION });
    return true;
  }

  // GET /api/v3/user
  if (rest.length === 1 && rest[0] === 'user') {
    if (method !== 'GET') {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    sendJson(ctx, 200, userJson());
    return true;
  }

  // /api/v3/user/repos
  if (rest.length === 2 && rest[0] === 'user' && rest[1] === 'repos') {
    if (method === 'POST') {
      const parsed = parseJsonBody(ctx);
      if (!parsed.ok) {
        sendErr(ctx, 400, 'Problems parsing JSON');
        return true;
      }
      const body = parsed.value;
      const name = typeof body.name === 'string' ? body.name : '';
      if (name === '' || !NAME_RE.test(name) || name.endsWith('.git')) {
        sendErr(ctx, 422, 'name is required and must be a valid repository name');
        return true;
      }
      if (ctx.store.getRepo(VIEWER_LOGIN, name)) {
        sendErr(ctx, 422, `name already exists on this account`);
        return true;
      }
      const repo = ctx.store.createRepo(
        VIEWER_LOGIN,
        name,
        body.private === true,
        typeof body.description === 'string' ? body.description : '',
      );
      sendJson(ctx, 201, repoJson(repo));
      return true;
    }
    if (method === 'GET') {
      const all = [...ctx.store.repos.values()]
        .filter((r) => r.owner === VIEWER_LOGIN)
        .sort((a, b) => a.id - b.id);
      const { page, link } = paginate(ctx, all, '/api/v3/user/repos');
      sendJson(ctx, 200, page.map(repoJson), link ? { link } : undefined);
      return true;
    }
    sendErr(ctx, 404, 'Not Found');
    return true;
  }

  // /api/v3/repos/{owner}/{repo}[...]
  if (rest.length >= 3 && rest[0] === 'repos') {
    const owner = decodeURIComponent(rest[1]);
    const name = decodeURIComponent(rest[2]);
    const tail = rest.slice(3);
    const repo = repoOr404(ctx, owner, name);
    if (repo === null) {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    return handleRepoRest(ctx, repo, tail);
  }

  sendErr(ctx, 404, 'Not Found');
  return true;
}

function handleRepoRest(ctx: Ctx, repo: Repo, tail: string[]): boolean {
  const method = ctx.method;
  const store = ctx.store;

  // /repos/{o}/{r} itself
  if (tail.length === 0) {
    if (method === 'GET') {
      sendJson(ctx, 200, repoJson(repo));
      return true;
    }
    if (method === 'PATCH') {
      const parsed = parseJsonBody(ctx);
      if (!parsed.ok) {
        sendErr(ctx, 400, 'Problems parsing JSON');
        return true;
      }
      const body = parsed.value;
      if (typeof body.description === 'string') repo.description = body.description;
      if (typeof body.private === 'boolean') repo.isPrivate = body.private;
      if (typeof body.default_branch === 'string' && NAME_RE.test(body.default_branch)) {
        repo.defaultBranch = body.default_branch;
      }
      repo.updatedAt = isoOf(store.nextTs());
      sendJson(ctx, 200, repoJson(repo));
      return true;
    }
    if (method === 'DELETE') {
      store.deleteRepo(repo.owner, repo.name);
      ctx.res.writeHead(204, { 'x-github-request-id': store.nextReqId() });
      ctx.res.end();
      return true;
    }
    sendErr(ctx, 404, 'Not Found');
    return true;
  }

  const head = tail[0];

  // /branches
  if (head === 'branches' && tail.length === 1) {
    if (method !== 'GET') {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    const branches = [...repo.refs.entries()]
      .filter(([ref]) => ref.startsWith('refs/heads/'))
      .map(([ref, sha]) => ({
        name: ref.slice('refs/heads/'.length),
        commit: { sha, url: `/api/v3/repos/${repo.owner}/${repo.name}/commits/${sha}` },
        protected: false,
      }))
      .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    sendJson(ctx, 200, branches);
    return true;
  }

  // /commits
  if (head === 'commits' && tail.length === 1) {
    if (method !== 'GET') {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    const shaParam = ctx.query.get('sha');
    let tip: string | undefined;
    if (shaParam) {
      if (isOid(shaParam)) tip = shaParam;
      else tip = repo.refs.get(`refs/heads/${shaParam}`);
    } else {
      tip = repo.refs.get(`refs/heads/${repo.defaultBranch}`);
    }
    if (!tip || !store.lookup(tip)) {
      sendJson(ctx, 200, []);
      return true;
    }
    const out: Record<string, unknown>[] = [];
    const seen = new Set<string>();
    const queue = [tip];
    while (queue.length) {
      const oid = queue.shift()!;
      if (seen.has(oid)) continue;
      seen.add(oid);
      const obj = store.lookup(oid);
      if (!obj || obj.type !== 'commit') continue;
      const commit = parseCommit(obj.data);
      out.push({
        sha: oid,
        commit: {
          message: commit.message,
          author: { name: VIEWER_NAME, email: VIEWER_EMAIL, date: isoOf(BASE_TS) },
          committer: { name: VIEWER_NAME, email: VIEWER_EMAIL, date: isoOf(BASE_TS) },
          tree: { sha: commit.tree },
        },
        url: `/api/v3/repos/${repo.owner}/${repo.name}/commits/${oid}`,
      });
      for (const parent of commit.parents) queue.push(parent);
    }
    sendJson(ctx, 200, out);
    return true;
  }

  // /git/refs
  if (head === 'git' && tail[1] === 'refs') {
    if (method === 'POST' && tail.length === 2) {
      const parsed = parseJsonBody(ctx);
      if (!parsed.ok) {
        sendErr(ctx, 400, 'Problems parsing JSON');
        return true;
      }
      const body = parsed.value;
      const ref = typeof body.ref === 'string' ? body.ref : '';
      const sha = typeof body.sha === 'string' ? body.sha : '';
      if (!ref.startsWith('refs/') || !isOid(sha) || !store.lookup(sha)) {
        sendErr(ctx, 422, 'Invalid ref or sha');
        return true;
      }
      if (repo.refs.has(ref)) {
        sendErr(ctx, 422, 'Reference already exists');
        return true;
      }
      repo.refs.set(ref, sha);
      sendJson(ctx, 201, { ref, object: { sha, type: 'commit' }, url: `/api/v3/repos/${repo.owner}/${repo.name}/git/refs/${ref}` });
      return true;
    }
    if (method === 'GET') {
      if (tail.length === 2) {
        const refs = [...repo.refs.entries()].map(([ref, sha]) => ({
          ref,
          object: { sha, type: 'commit' },
          url: `/api/v3/repos/${repo.owner}/${repo.name}/git/refs/${ref}`,
        }));
        sendJson(ctx, 200, refs);
        return true;
      }
      const refName = `refs/${tail.slice(2).map(decodeURIComponent).join('/')}`;
      const sha = repo.refs.get(refName);
      if (!sha) {
        sendErr(ctx, 404, 'Not Found');
        return true;
      }
      sendJson(ctx, 200, { ref: refName, object: { sha, type: 'commit' }, url: `/api/v3/repos/${repo.owner}/${repo.name}/git/refs/${refName}` });
      return true;
    }
    sendErr(ctx, 404, 'Not Found');
    return true;
  }

  // /contents/{path...}
  if (head === 'contents') {
    const segments = tail.slice(1).map(decodeURIComponent);
    return handleContents(ctx, repo, segments);
  }

  // /labels
  if (head === 'labels') {
    if (method === 'POST' && tail.length === 1) {
      const parsed = parseJsonBody(ctx);
      if (!parsed.ok) {
        sendErr(ctx, 400, 'Problems parsing JSON');
        return true;
      }
      const body = parsed.value;
      const name = typeof body.name === 'string' ? body.name : '';
      const color = typeof body.color === 'string' ? body.color : '000000';
      if (name === '') {
        sendErr(ctx, 422, 'name is required');
        return true;
      }
      if (repo.labels.has(name)) {
        sendErr(ctx, 422, 'already_exists');
        return true;
      }
      repo.labelCounter += 1;
      const label: Label = { id: repo.labelCounter, name, color };
      repo.labels.set(name, label);
      sendJson(ctx, 201, labelJson(label));
      return true;
    }
    if (method === 'GET' && tail.length === 1) {
      sendJson(ctx, 200, [...repo.labels.values()].map(labelJson));
      return true;
    }
    sendErr(ctx, 404, 'Not Found');
    return true;
  }

  // /issues
  if (head === 'issues') {
    if (tail.length === 1) {
      if (method === 'GET') {
        const state = ctx.query.get('state') ?? 'open';
        let list = [...repo.issues.values()];
        if (state !== 'all') list = list.filter((i) => i.state === state);
        list = sortIssues(list, ctx.query.get('sort') ?? 'created', ctx.query.get('direction') ?? 'asc');
        const { page, link } = paginate(ctx, list, `/api/v3/repos/${repo.owner}/${repo.name}/issues`);
        sendJson(ctx, 200, page.map((i) => issueJson(repo, i)), link ? { link } : undefined);
        return true;
      }
      if (method === 'POST') {
        const parsed = parseJsonBody(ctx);
        if (!parsed.ok) {
          sendErr(ctx, 400, 'Problems parsing JSON');
          return true;
        }
        const body = parsed.value;
        const title = typeof body.title === 'string' ? body.title : '';
        if (title === '') {
          sendErr(ctx, 422, 'title is required');
          return true;
        }
        const labels: string[] = Array.isArray(body.labels)
          ? body.labels.filter((l): l is string => typeof l === 'string')
          : [];
        for (const label of labels) {
          if (!repo.labels.has(label)) {
            sendErr(ctx, 422, `label '${label}' does not exist`);
            return true;
          }
        }
        const issue = store.createIssue(repo, title, typeof body.body === 'string' ? body.body : '', labels);
        sendJson(ctx, 201, issueJson(repo, issue));
        return true;
      }
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    const number = Number(tail[1]);
    const issue = Number.isInteger(number) ? repo.issues.get(number) : undefined;
    if (!issue) {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    if (tail.length === 2) {
      if (method === 'GET') {
        sendJson(ctx, 200, issueJson(repo, issue));
        return true;
      }
      if (method === 'PATCH') {
        const parsed = parseJsonBody(ctx);
        if (!parsed.ok) {
          sendErr(ctx, 400, 'Problems parsing JSON');
          return true;
        }
        const body = parsed.value;
        if (typeof body.state === 'string' && (body.state === 'open' || body.state === 'closed')) {
          issue.state = body.state;
          issue.closedAt = body.state === 'closed' ? isoOf(store.nextTs()) : null;
        }
        if (typeof body.title === 'string') issue.title = body.title;
        if (typeof body.body === 'string') issue.body = body.body;
        if (Array.isArray(body.labels)) {
          const labels = body.labels.filter((l): l is string => typeof l === 'string');
          for (const label of labels) {
            if (!repo.labels.has(label)) {
              sendErr(ctx, 422, `label '${label}' does not exist`);
              return true;
            }
          }
          issue.labels = labels;
        }
        issue.updatedAt = isoOf(store.nextTs());
        sendJson(ctx, 200, issueJson(repo, issue));
        return true;
      }
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    if (tail.length === 3 && tail[2] === 'comments') {
      if (method === 'GET') {
        sendJson(ctx, 200, issue.comments.map((c) => commentJson(repo, issue, c)));
        return true;
      }
      if (method === 'POST') {
        const parsed = parseJsonBody(ctx);
        if (!parsed.ok) {
          sendErr(ctx, 400, 'Problems parsing JSON');
          return true;
        }
        const body = parsed.value;
        const text = typeof body.body === 'string' ? body.body : '';
        if (text === '') {
          sendErr(ctx, 422, 'body is required');
          return true;
        }
        const comment = store.addComment(repo, issue, text);
        sendJson(ctx, 201, commentJson(repo, issue, comment));
        return true;
      }
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    sendErr(ctx, 404, 'Not Found');
    return true;
  }

  // /pulls
  if (head === 'pulls') {
    if (tail.length === 1) {
      if (method === 'GET') {
        const state = ctx.query.get('state') ?? 'open';
        let list = [...repo.pulls.values()];
        if (state !== 'all') list = list.filter((p) => p.state === state);
        list.sort((a, b) => a.number - b.number);
        const { page, link } = paginate(ctx, list, `/api/v3/repos/${repo.owner}/${repo.name}/pulls`);
        sendJson(ctx, 200, page.map((p) => prJson(store, repo, p)), link ? { link } : undefined);
        return true;
      }
      if (method === 'POST') {
        const parsed = parseJsonBody(ctx);
        if (!parsed.ok) {
          sendErr(ctx, 400, 'Problems parsing JSON');
          return true;
        }
        const body = parsed.value;
        const headRef = typeof body.head === 'string' ? body.head : '';
        const baseRef = typeof body.base === 'string' ? body.base : '';
        const title = typeof body.title === 'string' ? body.title : '';
        if (headRef === '' || baseRef === '' || title === '') {
          sendErr(ctx, 422, 'head, base and title are required');
          return true;
        }
        if (!repo.refs.has(`refs/heads/${headRef}`) || !repo.refs.has(`refs/heads/${baseRef}`)) {
          sendErr(ctx, 422, 'head or base branch does not exist');
          return true;
        }
        const pr = store.createPullRequest(repo, title, typeof body.body === 'string' ? body.body : '', headRef, baseRef);
        if (!pr) {
          sendErr(ctx, 422, 'could not create pull request');
          return true;
        }
        sendJson(ctx, 201, prJson(store, repo, pr));
        return true;
      }
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    const number = Number(tail[1]);
    const pr = Number.isInteger(number) ? repo.pulls.get(number) : undefined;
    if (!pr) {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    if (tail.length === 2) {
      if (method === 'GET') {
        sendJson(ctx, 200, prJson(store, repo, pr));
        return true;
      }
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    if (tail.length === 3 && tail[2] === 'files') {
      if (method !== 'GET') {
        sendErr(ctx, 404, 'Not Found');
        return true;
      }
      const baseTip = repo.refs.get(`refs/heads/${pr.baseRef}`) ?? pr.baseSha;
      const headTip = repo.refs.get(`refs/heads/${pr.headRef}`) ?? pr.headSha;
      const files = diffTrees(baseTip, headTip, store.lookup).map((f) => ({
        filename: f.filename,
        status: f.status === 'removed' ? 'removed' : f.status === 'added' ? 'added' : 'modified',
        additions: f.additions,
        deletions: f.deletions,
        changes: f.changes,
      }));
      sendJson(ctx, 200, files);
      return true;
    }
    if (tail.length === 3 && tail[2] === 'merge') {
      if (method !== 'PUT') {
        sendErr(ctx, 404, 'Not Found');
        return true;
      }
      const parsed = parseJsonBody(ctx);
      if (!parsed.ok) {
        sendErr(ctx, 400, 'Problems parsing JSON');
        return true;
      }
      const merged = mergePullRequest(store, repo, pr);
      if (merged === 'not-found') {
        sendErr(ctx, 404, 'Not Found');
        return true;
      }
      if (merged === 'already') {
        sendErr(ctx, 405, 'Pull request is already merged');
        return true;
      }
      if (merged === 'conflict') {
        sendErr(ctx, 405, 'Merge conflict');
        return true;
      }
      sendJson(ctx, 200, { merged: true, sha: merged, message: 'Pull request successfully merged' });
      return true;
    }
    sendErr(ctx, 404, 'Not Found');
    return true;
  }

  sendErr(ctx, 404, 'Not Found');
  return true;
}

/** Squash-merge a PR. Returns the merge commit oid, or 'already' | 'conflict' | 'not-found'. */
function mergePullRequest(store: Store, repo: Repo, pr: PullRequest): string | 'already' | 'conflict' | 'not-found' {
  if (pr.state !== 'open') return 'already';
  const baseTip = repo.refs.get(`refs/heads/${pr.baseRef}`);
  const headTip = repo.refs.get(`refs/heads/${pr.headRef}`);
  if (!baseTip || !headTip) return 'not-found';
  if (mergeConflicts(baseTip, headTip, store.lookup)) return 'conflict';
  const headTree = parseCommit(store.lookup(headTip)?.data ?? Buffer.alloc(0)).tree;
  const ts = store.nextTs();
  const message = `Merge pull request #${pr.number} from ${repo.owner}/${pr.headRef}\n\n${pr.title}`;
  const mergeOid = store.putObject('commit', buildCommit(headTree, [baseTip], message, ts));
  repo.refs.set(`refs/heads/${pr.baseRef}`, mergeOid);
  repo.updatedAt = isoOf(ts);
  repo.pushedAt = isoOf(ts);
  pr.state = 'closed';
  pr.merged = true;
  pr.mergedAt = isoOf(ts);
  pr.mergeCommitSha = mergeOid;
  pr.closedAt = isoOf(ts);
  pr.updatedAt = isoOf(ts);
  pr.baseSha = mergeOid;
  return mergeOid;
}

function handleContents(ctx: Ctx, repo: Repo, segments: string[]): boolean {
  const method = ctx.method;
  const store = ctx.store;

  const resolveRef = (): string | null => {
    const ref = ctx.query.get('ref');
    if (ref) {
      if (isOid(ref)) return store.lookup(ref) ? ref : null;
      return repo.refs.get(`refs/heads/${ref}`) ?? null;
    }
    return repo.refs.get(`refs/heads/${repo.defaultBranch}`) ?? null;
  };

  if (method === 'GET') {
    const tip = resolveRef();
    if (!tip) {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    const commitObj = store.lookup(tip);
    if (!commitObj || commitObj.type !== 'commit') {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    const rootTree = parseCommit(commitObj.data).tree;
    if (segments.length === 0) {
      const entries = listDir(rootTree, [], store.lookup) ?? [];
      sendJson(ctx, 200, entries.map((e) => ({
        type: e.mode === '40000' ? 'dir' : 'file',
        name: e.name,
        path: e.name,
        sha: e.oid,
        size: e.mode === '40000' ? 0 : (store.lookup(e.oid)?.data.length ?? 0),
      })));
      return true;
    }
    const dir = listDir(rootTree, segments, store.lookup);
    if (dir !== null) {
      const prefix = segments.join('/');
      sendJson(ctx, 200, dir.map((e) => ({
        type: e.mode === '40000' ? 'dir' : 'file',
        name: e.name,
        path: `${prefix}/${e.name}`,
        sha: e.oid,
        size: e.mode === '40000' ? 0 : (store.lookup(e.oid)?.data.length ?? 0),
      })));
      return true;
    }
    const blobOid = getPath(rootTree, segments, store.lookup);
    if (blobOid === null) {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    const blob = store.lookup(blobOid);
    if (!blob) {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    sendJson(ctx, 200, {
      type: 'file',
      encoding: 'base64',
      content: blob.data.toString('base64'),
      sha: blobOid,
      size: blob.data.length,
      name: segments[segments.length - 1],
      path: segments.join('/'),
    });
    return true;
  }

  if (method === 'PUT' || method === 'DELETE') {
    const parsed = parseJsonBody(ctx);
    if (!parsed.ok) {
      sendErr(ctx, 400, 'Problems parsing JSON');
      return true;
    }
    const body = parsed.value;
    const branch = typeof body.branch === 'string' && body.branch !== '' ? body.branch : repo.defaultBranch;
    const tip = repo.refs.get(`refs/heads/${branch}`);
    const rootTree = tip ? parseCommit(store.lookup(tip)?.data ?? Buffer.alloc(0)).tree : null;

    if (method === 'PUT') {
      const message = typeof body.message === 'string' ? body.message : 'Update file';
      const content = typeof body.content === 'string' ? body.content : '';
      if (content === '') {
        sendErr(ctx, 422, 'content is required');
        return true;
      }
      const bytes = b64decode(content);
      const existing = getPath(rootTree, segments, store.lookup);
      const sha = typeof body.sha === 'string' ? body.sha : '';
      if (existing !== null) {
        if (sha === '') {
          sendErr(ctx, 422, 'sha is required to update a file');
          return true;
        }
        if (sha !== existing) {
          sendErr(ctx, 409, 'sha does not match the current file');
          return true;
        }
      }
      const blobOid = store.putObject('blob', bytes);
      const newTree = setPath(rootTree, segments, blobOid, store.lookup, (t, d) => store.putObject(t, d));
      const parents = tip ? [tip] : [];
      const commitOid = store.commitOn(repo, branch, newTree, parents, message);
      sendJson(ctx, existing === null ? 201 : 200, {
        content: {
          name: segments[segments.length - 1],
          path: segments.join('/'),
          sha: blobOid,
          size: bytes.length,
          encoding: 'base64',
        },
        commit: { sha: commitOid, message },
      });
      return true;
    }

    // DELETE
    const message = typeof body.message === 'string' ? body.message : 'Delete file';
    const sha = typeof body.sha === 'string' ? body.sha : '';
    const existing = getPath(rootTree, segments, store.lookup);
    if (existing === null) {
      sendErr(ctx, 404, 'Not Found');
      return true;
    }
    if (sha === '' || sha !== existing) {
      sendErr(ctx, 409, 'sha does not match the current file');
      return true;
    }
    const newTree = deletePath(rootTree, segments, store.lookup, (t, d) => store.putObject(t, d));
    const parents = tip ? [tip] : [];
    const commitOid = store.commitOn(repo, branch, newTree, parents, message);
    sendJson(ctx, 200, { commit: { sha: commitOid, message } });
    return true;
  }

  sendErr(ctx, 404, 'Not Found');
  return true;
}

// -------------------------------------------------------------------------------------------
// git smart HTTP
// -------------------------------------------------------------------------------------------

function refEntries(repo: Repo): Array<{ name: string; oid: string }> {
  return [...repo.refs.keys()].sort().map((name) => ({ name, oid: repo.refs.get(name)! }));
}

function peeledOf(store: Store, oid: string): string | null {
  const obj = store.lookup(oid);
  if (obj?.type === 'tag') return parseTag(obj.data).object;
  return null;
}

function v2Advertisement(): Buffer {
  return pktLines(['version 2\n', 'ls-refs=unborn\n', 'fetch=shallow\n', 'object-format=sha1\n', 'flush']);
}

function v0Advertisement(store: Store, repo: Repo, service: string): Buffer {
  const caps =
    service === 'git-upload-pack'
      ? 'symref=HEAD:refs/heads/main side-band-64k shallow thin-pack ofs-delta multi_ack'
      : 'symref=HEAD:refs/heads/main report-status side-band-64k delete-refs';
  const parts: Buffer[] = [pktLine(`# service=${service}\n`), pktFlush()];
  const refs = refEntries(repo);
  if (refs.length === 0) {
    parts.push(pktLine(`${ZERO} capabilities^{}\0${caps}\n`));
  } else {
    const mainTip = repo.refs.get(`refs/heads/${repo.defaultBranch}`);
    if (mainTip) parts.push(pktLine(`${mainTip} HEAD\0${caps}\n`));
    else parts.push(pktLine(`${refs[0].oid} ${refs[0].name}\0${caps}\n`));
    for (const { name, oid } of refs) {
      parts.push(pktLine(`${oid} ${name}\n`));
      const peeled = peeledOf(store, oid);
      if (peeled) parts.push(pktLine(`${peeled} ${name}^{}\n`));
    }
  }
  parts.push(pktFlush());
  return Buffer.concat(parts);
}

function sendGit(ctx: Ctx, status: number, contentType: string, body: Buffer): void {
  ctx.res.writeHead(status, {
    'content-type': contentType,
    'x-github-request-id': ctx.store.nextReqId(),
  });
  ctx.res.end(body);
}

function handleInfoRefs(ctx: Ctx, owner: string, name: string, service: string): boolean {
  const repo = repoOr404(ctx, owner, name);
  if (!repo) {
    sendErr(ctx, 404, 'Not Found');
    return true;
  }
  const proto = (ctx.req.headers['git-protocol'] ?? '').toString();
  if (service === 'git-upload-pack') {
    if (proto.startsWith('version=2')) {
      sendGit(ctx, 200, 'application/x-git-upload-pack-advertisement', v2Advertisement());
    } else {
      sendGit(ctx, 200, 'application/x-git-upload-pack-advertisement', v0Advertisement(ctx.store, repo, service));
    }
    return true;
  }
  if (service === 'git-receive-pack') {
    sendGit(ctx, 200, 'application/x-git-receive-pack-advertisement', v0Advertisement(ctx.store, repo, service));
    return true;
  }
  sendErr(ctx, 404, 'Not Found');
  return true;
}

interface LsRefsArgs {
  peel: boolean;
  symrefs: boolean;
  unborn: boolean;
  prefixes: string[];
}

function parseLsRefsArgs(packets: PktPacket[]): LsRefsArgs {
  const args: LsRefsArgs = { peel: false, symrefs: false, unborn: false, prefixes: [] };
  let seenDelim = false;
  for (const packet of packets) {
    if (packet.kind === 'delim') {
      seenDelim = true;
      continue;
    }
    if (packet.kind !== 'data') continue;
    const line = packetText(packet);
    if (!seenDelim) continue;
    if (line === 'peel\n') args.peel = true;
    else if (line === 'symrefs\n') args.symrefs = true;
    else if (line === 'unborn\n') args.unborn = true;
    else if (line.startsWith('ref-prefix ')) args.prefixes.push(line.slice('ref-prefix '.length).replace(/\n$/, ''));
  }
  return args;
}

function lsRefsResponse(store: Store, repo: Repo, args: LsRefsArgs): Buffer {
  const lines: string[] = [];
  const keep = (name: string): boolean =>
    args.prefixes.length === 0 || args.prefixes.some((p) => name.startsWith(p));
  const mainTip = repo.refs.get(`refs/heads/${repo.defaultBranch}`);
  if (mainTip) {
    if (keep('HEAD')) {
      lines.push(args.symrefs ? `${mainTip} HEAD symref-target:refs/heads/${repo.defaultBranch}\n` : `${mainTip} HEAD\n`);
    }
  } else if (args.unborn) {
    if (keep('HEAD')) lines.push(`unborn HEAD symref-target:refs/heads/${repo.defaultBranch}\n`);
  }
  for (const { name, oid } of refEntries(repo)) {
    if (!keep(name)) continue;
    let line = `${oid} ${name}`;
    if (args.peel) {
      const peeled = peeledOf(store, oid);
      if (peeled) line += ` peeled:${peeled}`;
    }
    lines.push(`${line}\n`);
  }
  return pktLines([...lines, 'flush']);
}

interface FetchArgs {
  wants: string[];
  haves: string[];
  shallow: string[];
  depth: number | null;
}

function parseFetchArgs(packets: PktPacket[]): FetchArgs {
  const args: FetchArgs = { wants: [], haves: [], shallow: [], depth: null };
  let seenDelim = false;
  for (const packet of packets) {
    if (packet.kind === 'delim') {
      seenDelim = true;
      continue;
    }
    if (packet.kind !== 'data') continue;
    const line = packetText(packet);
    if (!seenDelim) continue;
    if (line.startsWith('want ')) args.wants.push(line.slice(5).replace(/\n$/, ''));
    else if (line.startsWith('have ')) args.haves.push(line.slice(5).replace(/\n$/, ''));
    else if (line.startsWith('shallow ')) args.shallow.push(line.slice(8).replace(/\n$/, ''));
    else if (line.startsWith('deepen ')) {
      const n = Number(line.slice(7).replace(/\n$/, ''));
      if (Number.isInteger(n) && n > 0) args.depth = n;
    }
  }
  return args;
}

/** The objects reachable from roots without following commit parents (commit -> tree, tag -> target, tree -> entries). */
function shallowClosure(roots: Iterable<string>, lookup: Lookup): Set<string> {
  const seen = new Set<string>();
  const queue: string[] = [];
  for (const root of roots) if (isOid(root)) queue.push(root);
  while (queue.length) {
    const oid = queue.pop()!;
    if (seen.has(oid)) continue;
    const object = lookup(oid);
    if (!object) continue;
    seen.add(oid);
    if (object.type === 'commit') {
      queue.push(parseCommit(object.data).tree);
    } else if (object.type === 'tag') {
      queue.push(parseTag(object.data).object);
    } else if (object.type === 'tree') {
      for (const entry of parseTree(object.data)) {
        if (entry.mode !== '160000') queue.push(entry.oid);
      }
    }
  }
  return seen;
}

/** The closure of the client's haves, stopping at the client's shallow boundaries: a shallow
 *  commit's parents are treated as not held (that is how a deepen fetch can deliver them). */
function haveClosure(roots: Iterable<string>, shallowSet: Set<string>, lookup: Lookup): Set<string> {
  const seen = new Set<string>();
  const queue: string[] = [];
  for (const root of roots) if (isOid(root)) queue.push(root);
  while (queue.length) {
    const oid = queue.pop()!;
    if (seen.has(oid)) continue;
    const object = lookup(oid);
    if (!object) continue;
    seen.add(oid);
    if (object.type === 'commit') {
      const commit = parseCommit(object.data);
      queue.push(commit.tree);
      if (!shallowSet.has(oid)) for (const parent of commit.parents) queue.push(parent);
    } else if (object.type === 'tag') {
      queue.push(parseTag(object.data).object);
    } else if (object.type === 'tree') {
      for (const entry of parseTree(object.data)) {
        if (entry.mode !== '160000') queue.push(entry.oid);
      }
    }
  }
  return seen;
}

/** The objects a fetch sends, plus the shallow boundary it reports. */
function computeFetch(
  store: Store,
  args: FetchArgs,
): { objects: GitObject[]; shallow: string[]; unshallow: string[]; acks: string[] } {
  const lookup = store.lookup;
  // Resolve wants to commit roots (a tag want resolves to its target commit).
  const commitRoots: string[] = [];
  const tagObjects: string[] = [];
  for (const want of args.wants) {
    const obj = lookup(want);
    if (!obj) continue;
    if (obj.type === 'tag') {
      tagObjects.push(want);
      const target = parseTag(obj.data).object;
      if (lookup(target)?.type === 'commit') commitRoots.push(target);
    } else if (obj.type === 'commit') {
      commitRoots.push(want);
    }
  }
  // BFS over parents from the roots, tracking distance.
  const dist = new Map<string, number>();
  const queue: Array<[string, number]> = commitRoots.map((r) => [r, 0] as [string, number]);
  const bfsCommits = new Set<string>();
  while (queue.length) {
    const [oid, d] = queue.shift()!;
    if (dist.has(oid)) continue;
    dist.set(oid, d);
    const obj = lookup(oid);
    if (!obj || obj.type !== 'commit') continue;
    if (args.depth !== null && d >= args.depth) continue;
    bfsCommits.add(oid);
    for (const parent of parseCommit(obj.data).parents) queue.push([parent, d + 1]);
  }
  const haveReachable = haveClosure(args.haves, new Set(args.shallow), lookup);
  // The send set is the depth-limited closure: each BFS commit contributes its tree and
  // blobs, but parents beyond the depth boundary are NOT followed (a depth-1 fetch sends
  // the tip only). With no depth limit the BFS already walked every parent, so this is
  // the full closure.
  const wantReachable = shallowClosure([...bfsCommits, ...tagObjects], lookup);
  const sendOids = [...wantReachable].filter((o) => !haveReachable.has(o)).sort();
  const objects: GitObject[] = [];
  for (const oid of sendOids) {
    const obj = lookup(oid);
    if (obj) objects.push(obj);
  }
  const sentCommits = new Set(sendOids.filter((o) => lookup(o)?.type === 'commit'));
  const acks: string[] = [];
  for (const have of args.haves) {
    if (lookup(have)?.type === 'commit') acks.push(have);
  }
  let shallow: string[] = [];
  let unshallow: string[] = [];
  if (args.depth !== null || args.shallow.length > 0) {
    const heldAfter = new Set<string>([...sentCommits, ...args.haves]);
    for (const oid of heldAfter) {
      const obj = lookup(oid);
      if (!obj || obj.type !== 'commit') continue;
      const parents = parseCommit(obj.data).parents;
      if (parents.length > 0 && parents.some((p) => !heldAfter.has(p))) shallow.push(oid);
    }
    shallow.sort();
    for (const oid of args.shallow) {
      const obj = lookup(oid);
      if (!obj || obj.type !== 'commit') continue;
      const parents = parseCommit(obj.data).parents;
      if (parents.length > 0 && parents.some((p) => sentCommits.has(p))) unshallow.push(oid);
    }
    unshallow.sort();
  }
  return { objects, shallow, unshallow, acks };
}

function sidebandChunks(pack: Buffer): Buffer[] {
  const chunks: Buffer[] = [];
  for (let offset = 0; offset < pack.length; offset += 32768) {
    const slice = pack.subarray(offset, Math.min(offset + 32768, pack.length));
    chunks.push(pktLine(Buffer.concat([Buffer.from([1]), slice])));
  }
  return chunks;
}

function handleUploadPack(ctx: Ctx, repo: Repo, protocolVersion: number): boolean {
  const { packets } = decodePktLines(ctx.body);
  const first = packets.find((p) => p.kind === 'data');
  if (!first) {
    sendErr(ctx, 400, 'Bad request: no git command');
    return true;
  }
  const command = packetText(first).replace(/\n$/, '');
  const store = ctx.store;

  if (command === 'command=ls-refs') {
    const args = parseLsRefsArgs(packets);
    sendGit(ctx, 200, 'application/x-git-upload-pack-result', lsRefsResponse(store, repo, args));
    return true;
  }

  if (command === 'command=fetch') {
    const args = parseFetchArgs(packets);
    const { objects, shallow, unshallow, acks } = computeFetch(store, args);
    if (protocolVersion === 2) {
      const parts: Buffer[] = [];
      parts.push(pktLine('acknowledgments\n'));
      if (acks.length) {
        for (const ack of acks) parts.push(pktLine(`ACK ${ack}\n`));
        parts.push(pktLine('ready\n'));
      } else {
        parts.push(pktLine('NAK\n'));
      }
      parts.push(pktFlush());
      if (shallow.length || unshallow.length) {
        parts.push(pktLine('shallow-info\n'));
        for (const oid of shallow) parts.push(pktLine(`shallow ${oid}\n`));
        for (const oid of unshallow) parts.push(pktLine(`unshallow ${oid}\n`));
        parts.push(pktFlush());
      }
      if (objects.length) {
        const pack = writePack(objects);
        parts.push(pktLine('packfile\n'));
        for (const chunk of sidebandChunks(pack)) parts.push(chunk);
        parts.push(pktFlush());
      }
      sendGit(ctx, 200, 'application/x-git-upload-pack-result', Buffer.concat(parts));
      return true;
    }
    // v0: bare NAK/ACK/shallow lines, then the sideband pack.
    const parts: Buffer[] = [];
    if (acks.length) {
      for (const ack of acks) parts.push(pktLine(`ACK ${ack}\n`));
    } else {
      parts.push(pktLine('NAK\n'));
    }
    for (const oid of shallow) parts.push(pktLine(`shallow ${oid}\n`));
    for (const oid of unshallow) parts.push(pktLine(`unshallow ${oid}\n`));
    if (objects.length) {
      const pack = writePack(objects);
      for (const chunk of sidebandChunks(pack)) parts.push(chunk);
    }
    sendGit(ctx, 200, 'application/x-git-upload-pack-result', Buffer.concat(parts));
    return true;
  }

  sendErr(ctx, 400, 'Bad request: unknown git command');
  return true;
}

interface PushCommand {
  oldOid: string;
  newOid: string;
  ref: string;
}

function handleReceivePack(ctx: Ctx, repo: Repo): boolean {
  const store = ctx.store;
  const { packets, rest } = decodePktLines(ctx.body);
  const dataPackets = packets.filter((p) => p.kind === 'data');
  if (!dataPackets.length) {
    sendErr(ctx, 400, 'Bad request: no push commands');
    return true;
  }
  const commands: PushCommand[] = [];
  let malformed = false;
  for (const packet of dataPackets) {
    let text = packetText(packet).replace(/\n$/, '');
    const nul = text.indexOf('\0');
    if (nul >= 0) text = text.slice(0, nul);
    const parts = text.split(' ');
    if (parts.length !== 3 || !isOid(parts[0]) || !isOid(parts[1]) || !parts[2].startsWith('refs/')) {
      malformed = true;
      break;
    }
    commands.push({ oldOid: parts[0], newOid: parts[1], ref: parts[2] });
  }

  const reportLines: string[] = [];
  let unpackOk = true;
  if (!commands.length) {
    unpackOk = false;
    reportLines.push('unpack error no commands');
  } else if (malformed) {
    unpackOk = false;
    reportLines.push('unpack error malformed command');
    for (const cmd of commands) reportLines.push(`ng ${cmd.ref} malformed command`);
  } else {
    const needsPack = commands.some((c) => c.newOid !== ZERO);
    if (needsPack && rest.length > 0) {
      try {
        const objects = readPack(rest, store.lookup);
        for (const [oid, obj] of objects) {
          if (!store.objects.has(oid)) store.objects.set(oid, obj);
        }
      } catch (err) {
        unpackOk = false;
        reportLines.push(`unpack error ${err instanceof Error ? err.message : 'bad pack'}`);
      }
    } else if (needsPack && rest.length === 0) {
      unpackOk = false;
      reportLines.push('unpack error missing pack');
    }
    if (unpackOk) {
      reportLines.push('unpack ok');
      for (const cmd of commands) {
        const current = repo.refs.get(cmd.ref);
        if (cmd.newOid === ZERO) {
          // delete
          if (current === undefined) {
            reportLines.push(`ng ${cmd.ref} ref does not exist`);
          } else if (cmd.oldOid !== ZERO && cmd.oldOid !== current) {
            reportLines.push(`ng ${cmd.ref} old value does not match`);
          } else {
            repo.refs.delete(cmd.ref);
            reportLines.push(`ok ${cmd.ref}`);
          }
        } else if (cmd.oldOid === ZERO) {
          // create
          if (current !== undefined) {
            reportLines.push(`ng ${cmd.ref} already exists`);
          } else if (!store.lookup(cmd.newOid)) {
            reportLines.push(`ng ${cmd.ref} missing object ${cmd.newOid}`);
          } else {
            repo.refs.set(cmd.ref, cmd.newOid);
            reportLines.push(`ok ${cmd.ref}`);
          }
        } else {
          // update
          if (current === undefined) {
            reportLines.push(`ng ${cmd.ref} ref does not exist`);
          } else if (cmd.oldOid !== current) {
            reportLines.push(`ng ${cmd.ref} old value does not match`);
          } else if (!store.lookup(cmd.newOid)) {
            reportLines.push(`ng ${cmd.ref} missing object ${cmd.newOid}`);
          } else if (!isDescendant(cmd.newOid, cmd.oldOid, store.lookup)) {
            reportLines.push(`ng ${cmd.ref} non-fast-forward`);
          } else {
            repo.refs.set(cmd.ref, cmd.newOid);
            reportLines.push(`ok ${cmd.ref}`);
          }
        }
      }
      const ts = store.nextTs();
      repo.updatedAt = isoOf(ts);
      repo.pushedAt = isoOf(ts);
    } else {
      for (const cmd of commands) reportLines.push(`ng ${cmd.ref} unpack failed`);
    }
  }

  const reportStream = pktLines([...reportLines.map((l) => `${l}\n`), 'flush']);
  const body = Buffer.concat([pktLine(Buffer.concat([Buffer.from([1]), reportStream])), pktFlush()]);
  sendGit(ctx, 200, 'application/x-git-receive-pack-result', body);
  return true;
}

function handleGit(ctx: Ctx): boolean {
  const seg = ctx.path.split('/').filter((s) => s !== '');
  // /{owner}/{repo}.git/info/refs  or  /{owner}/{repo}.git/git-upload-pack|git-receive-pack
  if (seg.length < 2 || !seg[1].endsWith('.git')) return false;
  const owner = decodeURIComponent(seg[0]);
  const name = decodeURIComponent(seg[1].slice(0, -4));
  if (!authorized(ctx)) {
    sendErr(ctx, 401, 'Requires authentication');
    return true;
  }
  const repo = repoOr404(ctx, owner, name);
  if (!repo) {
    sendErr(ctx, 404, 'Not Found');
    return true;
  }
  if (ctx.method === 'GET' && seg.length === 3 && seg[2] === 'info' ) {
    sendErr(ctx, 404, 'Not Found');
    return true;
  }
  if (ctx.method === 'GET' && seg.length === 4 && seg[2] === 'info' && seg[3] === 'refs') {
    const service = ctx.query.get('service') ?? 'git-upload-pack';
    return handleInfoRefs(ctx, owner, name, service);
  }
  if (ctx.method === 'POST' && seg.length === 3 && seg[2] === 'git-upload-pack') {
    const proto = (ctx.req.headers['git-protocol'] ?? '').toString();
    return handleUploadPack(ctx, repo, proto.startsWith('version=2') ? 2 : 0);
  }
  if (ctx.method === 'POST' && seg.length === 3 && seg[2] === 'git-receive-pack') {
    return handleReceivePack(ctx, repo);
  }
  sendErr(ctx, 404, 'Not Found');
  return true;
}

// -------------------------------------------------------------------------------------------
// GraphQL v4 (a small executor for exactly the documents the suite sends)
// -------------------------------------------------------------------------------------------

type GqlValue =
  | { kind: 'string'; value: string }
  | { kind: 'int'; value: number }
  | { kind: 'float'; value: number }
  | { kind: 'bool'; value: boolean }
  | { kind: 'null' }
  | { kind: 'enum'; value: string }
  | { kind: 'var'; name: string }
  | { kind: 'list'; items: GqlValue[] }
  | { kind: 'object'; fields: Array<{ name: string; value: GqlValue }> };

interface GqlField {
  alias: string | null;
  name: string;
  args: Record<string, GqlValue>;
  selections: GqlSelection[] | null;
}
type GqlSelection = GqlField | { fragment: true; typeCondition: string; selections: GqlSelection[] };

interface GqlOperation {
  kind: 'query' | 'mutation';
  name: string | null;
  varDefs: Map<string, GqlValue | undefined>;
  selections: GqlSelection[];
}

function tokenizeGql(src: string): Array<{ kind: string; value: string }> {
  const tokens: Array<{ kind: string; value: string }> = [];
  let i = 0;
  while (i < src.length) {
    const ch = src[i];
    if (ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r' || ch === ',') {
      i += 1;
      continue;
    }
    if (ch === '#') {
      while (i < src.length && src[i] !== '\n') i += 1;
      continue;
    }
    if (ch === '.' && src.slice(i, i + 3) === '...') {
      tokens.push({ kind: 'spread', value: '...' });
      i += 3;
      continue;
    }
    if ('{}()[]:!=$@&|'.includes(ch)) {
      tokens.push({ kind: 'punct', value: ch });
      i += 1;
      continue;
    }
    if (ch === '"') {
      let j = i + 1;
      let out = '';
      while (j < src.length && src[j] !== '"') {
        if (src[j] === '\\') {
          const esc = src[j + 1];
          if (esc === 'n') out += '\n';
          else if (esc === 't') out += '\t';
          else if (esc === 'r') out += '\r';
          else if (esc === 'b') out += '\b';
          else if (esc === 'f') out += '\f';
          else if (esc === 'u') {
            out += String.fromCharCode(Number.parseInt(src.slice(j + 2, j + 6), 16));
            j += 4;
          } else out += esc ?? '';
          j += 2;
        } else {
          out += src[j];
          j += 1;
        }
      }
      tokens.push({ kind: 'string', value: out });
      i = j + 1;
      continue;
    }
    if (/[0-9-]/.test(ch)) {
      let j = i;
      while (j < src.length && /[0-9.eE+-]/.test(src[j])) j += 1;
      const raw = src.slice(i, j);
      tokens.push({ kind: raw.includes('.') || raw.includes('e') || raw.includes('E') ? 'float' : 'int', value: raw });
      i = j;
      continue;
    }
    if (/[A-Za-z_]/.test(ch)) {
      let j = i;
      while (j < src.length && /[A-Za-z0-9_]/.test(src[j])) j += 1;
      tokens.push({ kind: 'name', value: src.slice(i, j) });
      i = j;
      continue;
    }
    throw new Error(`unexpected GraphQL character ${JSON.stringify(ch)}`);
  }
  tokens.push({ kind: 'eof', value: '' });
  return tokens;
}

class GqlParser {
  private pos = 0;
  private tokens: Array<{ kind: string; value: string }>;
  constructor(tokens: Array<{ kind: string; value: string }>) {
    this.tokens = tokens;
  }

  private peek(): { kind: string; value: string } {
    return this.tokens[this.pos];
  }

  private next(): { kind: string; value: string } {
    return this.tokens[this.pos++];
  }

  private expect(kind: string, value?: string): { kind: string; value: string } {
    const tok = this.next();
    if (tok.kind !== kind || (value !== undefined && tok.value !== value)) {
      throw new Error(`expected ${kind} ${value ?? ''}, got ${tok.kind} ${tok.value}`);
    }
    return tok;
  }

  private expectName(value?: string): string {
    const tok = this.expect('name');
    if (value !== undefined && tok.value !== value) throw new Error(`expected ${value}, got ${tok.value}`);
    return tok.value;
  }

  parseDocument(): GqlOperation[] {
    const ops: GqlOperation[] = [];
    while (this.peek().kind !== 'eof') {
      ops.push(this.parseOperation());
    }
    return ops;
  }

  private parseOperation(): GqlOperation {
    let kind: 'query' | 'mutation' = 'query';
    let name: string | null = null;
    const varDefs = new Map<string, GqlValue | undefined>();
    const tok = this.peek();
    if (tok.kind === 'name' && (tok.value === 'query' || tok.value === 'mutation')) {
      kind = this.next().value as 'query' | 'mutation';
      if (this.peek().kind === 'name') name = this.next().value;
    }
    if (this.peek().kind === 'punct' && this.peek().value === '(') {
      this.next();
      while (!(this.peek().kind === 'punct' && this.peek().value === ')')) {
        this.expect('punct', '$');
        const varName = this.expectName();
        this.expect('punct', ':');
        this.parseType();
        let def: GqlValue | undefined;
        if (this.peek().kind === 'punct' && this.peek().value === '=') {
          this.next();
          def = this.parseValue();
        }
        varDefs.set(varName, def);
      }
      this.expect('punct', ')');
    }
    const selections = this.parseSelectionSet();
    return { kind, name, varDefs, selections };
  }

  private parseType(): void {
    if (this.peek().kind === 'punct' && this.peek().value === '[') {
      this.next();
      this.parseType();
      this.expect('punct', ']');
    } else {
      this.expectName();
    }
    if (this.peek().kind === 'punct' && this.peek().value === '!') this.next();
  }

  private parseSelectionSet(): GqlSelection[] {
    this.expect('punct', '{');
    const selections: GqlSelection[] = [];
    while (!(this.peek().kind === 'punct' && this.peek().value === '}')) {
      selections.push(this.parseSelection());
    }
    this.expect('punct', '}');
    return selections;
  }

  private parseSelection(): GqlSelection {
    if (this.peek().kind === 'spread') {
      this.next();
      this.expectName('on');
      const typeCondition = this.expectName();
      const selections = this.parseSelectionSet();
      return { fragment: true, typeCondition, selections };
    }
    let alias: string | null = null;
    let name = this.expectName();
    if (this.peek().kind === 'punct' && this.peek().value === ':') {
      this.next();
      alias = name;
      name = this.expectName();
    }
    const args: Record<string, GqlValue> = {};
    if (this.peek().kind === 'punct' && this.peek().value === '(') {
      this.next();
      while (!(this.peek().kind === 'punct' && this.peek().value === ')')) {
        const argName = this.expectName();
        this.expect('punct', ':');
        args[argName] = this.parseValue();
      }
      this.expect('punct', ')');
    }
    let selections: GqlSelection[] | null = null;
    if (this.peek().kind === 'punct' && this.peek().value === '{') {
      selections = this.parseSelectionSet();
    }
    return { alias, name, args, selections };
  }

  private parseValue(): GqlValue {
    const tok = this.next();
    if (tok.kind === 'string') return { kind: 'string', value: tok.value };
    if (tok.kind === 'int') return { kind: 'int', value: Number(tok.value) };
    if (tok.kind === 'float') return { kind: 'float', value: Number(tok.value) };
    if (tok.kind === 'name') {
      if (tok.value === 'true') return { kind: 'bool', value: true };
      if (tok.value === 'false') return { kind: 'bool', value: false };
      if (tok.value === 'null') return { kind: 'null' };
      return { kind: 'enum', value: tok.value };
    }
    if (tok.kind === 'punct' && tok.value === '$') return { kind: 'var', name: this.expectName() };
    if (tok.kind === 'punct' && tok.value === '[') {
      const items: GqlValue[] = [];
      while (!(this.peek().kind === 'punct' && this.peek().value === ']')) items.push(this.parseValue());
      this.expect('punct', ']');
      return { kind: 'list', items };
    }
    if (tok.kind === 'punct' && tok.value === '{') {
      const fields: Array<{ name: string; value: GqlValue }> = [];
      while (!(this.peek().kind === 'punct' && this.peek().value === '}')) {
        const name = this.expectName();
        this.expect('punct', ':');
        fields.push({ name, value: this.parseValue() });
      }
      this.expect('punct', '}');
      return { kind: 'object', fields };
    }
    throw new Error(`unexpected GraphQL value ${tok.kind} ${tok.value}`);
  }
}

function evalGqlValue(value: GqlValue, variables: Record<string, unknown>): unknown {
  switch (value.kind) {
    case 'string':
    case 'int':
    case 'float':
    case 'bool':
      return value.value;
    case 'null':
      return null;
    case 'enum':
      return value.value;
    case 'var': {
      const v = variables[value.name];
      return v === undefined ? null : v;
    }
    case 'list':
      return value.items.map((item) => evalGqlValue(item, variables));
    case 'object': {
      const out: Record<string, unknown> = {};
      for (const field of value.fields) out[field.name] = evalGqlValue(field.value, variables);
      return out;
    }
  }
}

// The schema the suite introspects (fields the checks read, plus the introspection surface).
const GQL_FIELDS: Record<string, string[]> = {
  Query: ['viewer', 'repository', 'node', '__type'],
  Mutation: ['createIssue', 'closeIssue', 'createPullRequest', 'mergePullRequest', 'addComment'],
  CreateIssuePayload: ['issue'],
  CloseIssuePayload: ['issue'],
  CreatePullRequestPayload: ['pullRequest'],
  MergePullRequestPayload: ['pullRequest'],
  AddCommentPayload: ['commentEdge'],
  Viewer: ['login', 'id', 'name', 'email', 'databaseId', 'avatarUrl'],
  Repository: ['id', 'databaseId', 'name', 'nameWithOwner', 'owner', 'defaultBranchRef', 'viewerPermission', 'hasIssuesEnabled', 'issue', 'issues', 'pullRequests', 'pullRequest'],
  User: ['login', 'id', 'name', 'email', 'databaseId'],
  Ref: ['name', 'target'],
  Issue: ['id', 'number', 'title', 'body', 'state', 'url', 'createdAt', 'author'],
  IssueConnection: ['nodes', 'edges', 'pageInfo', 'totalCount'],
  IssueEdge: ['node', 'cursor'],
  PullRequestEdge: ['node', 'cursor'],
  PullRequestFileEdge: ['node', 'cursor'],
  PageInfo: ['hasNextPage', 'hasPreviousPage', 'startCursor', 'endCursor'],
  PullRequest: ['id', 'number', 'title', 'body', 'state', 'headRefName', 'baseRefName', 'mergeable', 'merged', 'files', 'headRef', 'baseRef', 'author'],
  PullRequestConnection: ['nodes', 'edges', 'pageInfo', 'totalCount'],
  PullRequestFileConnection: ['nodes', 'edges', 'pageInfo', 'totalCount'],
  PullRequestFile: ['path', 'additions', 'deletions', 'changeType'],
  IssueComment: ['id', 'body', 'author', 'createdAt'],
  IssueCommentEdge: ['node', 'cursor'],
  IssueCommentConnection: ['nodes', 'edges', 'pageInfo', 'totalCount'],
  __Type: ['kind', 'name', 'description', 'fields', 'enumValues', 'inputFields', 'interfaces', 'possibleTypes', 'ofType'],
  __Field: ['name', 'description', 'args', 'type', 'isDeprecated', 'deprecationReason'],
  __InputValue: ['name', 'description', 'type', 'defaultValue'],
  __EnumValue: ['name', 'description', 'isDeprecated', 'deprecationReason'],
};

const GQL_ENUMS: Record<string, string[]> = {
  SearchType: ['ISSUE', 'REPOSITORY', 'USER', 'CODE', 'DISCUSSION', 'COMMIT'],
  IssueState: ['OPEN', 'CLOSED'],
  MergeableState: ['MERGEABLE', 'CONFLICTING', 'UNKNOWN'],
  PullRequestState: ['OPEN', 'CLOSED', 'MERGED'],
  PullRequestMergeMethod: ['MERGE', 'SQUASH', 'REBASE'],
  RepositoryPermission: ['ADMIN', 'MAINTAIN', 'WRITE', 'TRIAGE', 'READ'],
  IssueOrderField: ['CREATED_AT', 'UPDATED_AT', 'COMMENTS'],
  OrderDirection: ['ASC', 'DESC'],
};

interface GqlObj {
  __type: string;
  [key: string]: unknown;
}

function decodeGlobalId(id: string): { kind: string; parts: string[] } | null {
  try {
    const text = Buffer.from(id, 'base64').toString('utf8');
    const parts = text.split(':');
    if (parts.length < 2) return null;
    return { kind: parts[0], parts: parts.slice(1) };
  } catch {
    return null;
  }
}

function cursorOf(index: number): string {
  return b64encode(`cursor:${index}`);
}

function indexOfCursor(cursor: string | null | undefined): number {
  if (!cursor) return -1;
  try {
    const text = Buffer.from(cursor, 'base64').toString('utf8');
    if (text.startsWith('cursor:')) {
      const n = Number(text.slice('cursor:'.length));
      return Number.isInteger(n) ? n : -1;
    }
  } catch {
    return -1;
  }
  return -1;
}

class GraphQLExecutor {
  private store: Store;
  constructor(store: Store) {
    this.store = store;
  }

  execute(query: string, variables: Record<string, unknown>): { data: Record<string, unknown> | null; errors?: Array<{ message: string }> } {
    let operations: GqlOperation[];
    try {
      operations = new GqlParser(tokenizeGql(query)).parseDocument();
    } catch (err) {
      return { data: null, errors: [{ message: err instanceof Error ? err.message : 'syntax error' }] };
    }
    const op = operations[0];
    if (!op) return { data: null, errors: [{ message: 'empty document' }] };
    const errors: Array<{ message: string }> = [];
    const rootType = op.kind === 'mutation' ? 'Mutation' : 'Query';
    const data: Record<string, unknown> = {};
    for (const selection of op.selections) {
      if ('fragment' in selection) continue;
      const field = selection as GqlField;
      const key = field.alias ?? field.name;
      try {
        data[key] = this.resolveRoot(rootType, field, variables, errors);
      } catch (err) {
        errors.push({ message: err instanceof Error ? err.message : String(err) });
        data[key] = null;
      }
    }
    return errors.length ? { data, errors } : { data };
  }

  private resolveRoot(rootType: string, field: GqlField, variables: Record<string, unknown>, errors: Array<{ message: string }>): unknown {
    const args = this.evalArgs(field, variables);
    if (rootType === 'Query') {
      if (field.name === 'viewer') {
        return this.resolveFields('Viewer', this.viewerObj(), field.selections, variables, errors);
      }
      if (field.name === 'repository') {
        const owner = String(args.owner ?? '');
        const name = String(args.name ?? '');
        const repo = owner === VIEWER_LOGIN ? this.store.getRepo(owner, name) : undefined;
        if (!repo) return null;
        return this.resolveFields('Repository', this.repoObj(repo), field.selections, variables, errors);
      }
      if (field.name === 'node') {
        const decoded = typeof args.id === 'string' ? decodeGlobalId(args.id) : null;
        if (!decoded) return null;
        const obj = this.nodeObj(decoded);
        if (!obj) return null;
        return this.resolveFields(obj.__type, obj, field.selections, variables, errors);
      }
      if (field.name === '__type') {
        const name = String(args.name ?? '');
        const typeObj = this.typeObj(name);
        if (!typeObj) return null;
        return this.resolveFields('__Type', typeObj, field.selections, variables, errors);
      }
    } else {
      const mutation = this.resolveMutation(field.name, args);
      if (mutation === undefined) {
        errors.push({ message: `Cannot query field "${field.name}" on type "Mutation".` });
        return null;
      }
      if (mutation === null) return null;
      return this.resolveFields(mutation.__type, mutation, field.selections, variables, errors);
    }
    errors.push({ message: `Cannot query field "${field.name}" on type "${rootType}".` });
    return null;
  }

  private viewerObj(): GqlObj {
    return { __type: 'Viewer', login: VIEWER_LOGIN, id: String(VIEWER_ID), databaseId: VIEWER_ID, name: VIEWER_NAME, email: VIEWER_EMAIL, avatarUrl: `https://example.invalid/avatar/${VIEWER_LOGIN}` };
  }

  private userObj(): GqlObj {
    return { __type: 'User', login: VIEWER_LOGIN, id: String(VIEWER_ID), databaseId: VIEWER_ID, name: VIEWER_NAME, email: VIEWER_EMAIL };
  }

  private repoObj(repo: Repo): GqlObj {
    return { __type: 'Repository', repo, id: gid('Repository', repo.id), databaseId: repo.id, name: repo.name, nameWithOwner: `${repo.owner}/${repo.name}`, owner: this.userObj(), defaultBranchRef: { __type: 'Ref', name: repo.defaultBranch, target: { __type: 'Commit', oid: repo.refs.get(`refs/heads/${repo.defaultBranch}`) ?? '' } }, viewerPermission: 'ADMIN', hasIssuesEnabled: true };
  }

  private issueObj(repo: Repo, issue: Issue): GqlObj {
    return { __type: 'Issue', repo, issue, id: gid('Issue', repo.id, issue.number), number: issue.number, title: issue.title, body: issue.body, state: issue.state.toUpperCase(), url: `https://example.invalid/${repo.owner}/${repo.name}/issues/${issue.number}`, createdAt: issue.createdAt, author: this.userObj() };
  }

  private prObj(repo: Repo, pr: PullRequest): GqlObj {
    const baseTip = repo.refs.get(`refs/heads/${pr.baseRef}`) ?? pr.baseSha;
    const headTip = repo.refs.get(`refs/heads/${pr.headRef}`) ?? pr.headSha;
    const state = pr.merged ? 'MERGED' : pr.state.toUpperCase();
    return { __type: 'PullRequest', repo, pr, id: gid('PullRequest', repo.id, pr.number), number: pr.number, title: pr.title, body: pr.body, state, headRefName: pr.headRef, baseRefName: pr.baseRef, mergeable: prMergeable(this.store, repo, pr) ? 'MERGEABLE' : 'CONFLICTING', merged: pr.merged, headRef: { __type: 'Ref', name: pr.headRef, target: { __type: 'Commit', oid: headTip } }, baseRef: { __type: 'Ref', name: pr.baseRef, target: { __type: 'Commit', oid: baseTip } }, author: this.userObj() };
  }

  private commentObj(comment: Comment): GqlObj {
    return { __type: 'IssueComment', id: gid('IssueComment', comment.id), body: comment.body, createdAt: comment.createdAt, author: this.userObj() };
  }

  private nodeObj(decoded: { kind: string; parts: string[] }): GqlObj | null {
    if (decoded.kind === 'Repository') {
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      return repo ? this.repoObj(repo) : null;
    }
    if (decoded.kind === 'Issue') {
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      const issue = repo?.issues.get(Number(decoded.parts[1]));
      return repo && issue ? this.issueObj(repo, issue) : null;
    }
    if (decoded.kind === 'PullRequest') {
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      const pr = repo?.pulls.get(Number(decoded.parts[1]));
      return repo && pr ? this.prObj(repo, pr) : null;
    }
    if (decoded.kind === 'IssueComment') {
      for (const repo of this.store.repos.values()) {
        for (const issue of repo.issues.values()) {
          const comment = issue.comments.find((c) => c.id === Number(decoded.parts[0]));
          if (comment) return this.commentObj(comment);
        }
      }
      return null;
    }
    return null;
  }

  private typeObj(name: string): GqlObj | null {
    if (GQL_FIELDS[name]) {
      return { __type: '__Type', kind: 'OBJECT', name, fields: GQL_FIELDS[name], enumValues: null };
    }
    if (GQL_ENUMS[name]) {
      return { __type: '__Type', kind: 'ENUM', name, fields: null, enumValues: GQL_ENUMS[name] };
    }
    return null;
  }

  private evalArgs(field: GqlField, variables: Record<string, unknown>): Record<string, unknown> {
    const out: Record<string, unknown> = {};
    for (const [name, value] of Object.entries(field.args)) out[name] = evalGqlValue(value, variables);
    return out;
  }

  private resolveMutation(name: string, args: Record<string, unknown>): GqlObj | null | undefined {
    const input = (args.input ?? {}) as Record<string, unknown>;
    if (name === 'createIssue') {
      const decoded = typeof input.repositoryId === 'string' ? decodeGlobalId(input.repositoryId) : null;
      if (!decoded || decoded.kind !== 'Repository') return null;
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      if (!repo) return null;
      const issue = this.store.createIssue(repo, String(input.title ?? ''), typeof input.body === 'string' ? input.body : '', []);
      return { __type: 'CreateIssuePayload', issue: this.issueObj(repo, issue) };
    }
    if (name === 'closeIssue') {
      const decoded = typeof input.issueId === 'string' ? decodeGlobalId(input.issueId) : null;
      if (!decoded || decoded.kind !== 'Issue') return null;
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      const issue = repo?.issues.get(Number(decoded.parts[1]));
      if (!repo || !issue) return null;
      issue.state = 'closed';
      issue.closedAt = isoOf(this.store.nextTs());
      return { __type: 'CloseIssuePayload', issue: this.issueObj(repo, issue) };
    }
    if (name === 'createPullRequest') {
      const decoded = typeof input.repositoryId === 'string' ? decodeGlobalId(input.repositoryId) : null;
      if (!decoded || decoded.kind !== 'Repository') return null;
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      if (!repo) return null;
      const pr = this.store.createPullRequest(repo, String(input.title ?? ''), typeof input.body === 'string' ? input.body : '', String(input.headRefName ?? ''), String(input.baseRefName ?? ''));
      if (!pr) return null;
      return { __type: 'CreatePullRequestPayload', pullRequest: this.prObj(repo, pr) };
    }
    if (name === 'mergePullRequest') {
      const decoded = typeof input.pullRequestId === 'string' ? decodeGlobalId(input.pullRequestId) : null;
      if (!decoded || decoded.kind !== 'PullRequest') return null;
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      const pr = repo?.pulls.get(Number(decoded.parts[1]));
      if (!repo || !pr) return null;
      const merged = mergePullRequest(this.store, repo, pr);
      if (typeof merged !== 'string') return { __type: 'MergePullRequestPayload', pullRequest: this.prObj(repo, pr) };
      return { __type: 'MergePullRequestPayload', pullRequest: this.prObj(repo, pr) };
    }
    if (name === 'addComment') {
      const decoded = typeof input.subjectId === 'string' ? decodeGlobalId(input.subjectId) : null;
      if (!decoded || decoded.kind !== 'Issue') return null;
      const repo = this.store.getRepoById(Number(decoded.parts[0]));
      const issue = repo?.issues.get(Number(decoded.parts[1]));
      if (!repo || !issue) return null;
      const comment = this.store.addComment(repo, issue, String(input.body ?? ''));
      return { __type: 'AddCommentPayload', commentEdge: { __type: 'IssueCommentEdge', node: this.commentObj(comment), cursor: cursorOf(issue.comments.length - 1) } };
    }
    return undefined;
  }

  private resolveFields(
    typeName: string,
    obj: GqlObj,
    selections: GqlSelection[] | null,
    variables: Record<string, unknown>,
    errors: Array<{ message: string }>,
  ): Record<string, unknown> | null {
    if (!selections) return null;
    const out: Record<string, unknown> = {};
    for (const selection of selections) {
      if ('fragment' in selection) {
        if (selection.typeCondition === typeName || selection.typeCondition === 'Node') {
          const nested = this.resolveFields(typeName, obj, selection.selections, variables, errors);
          if (nested) Object.assign(out, nested);
        }
        continue;
      }
      const field = selection as GqlField;
      const key = field.alias ?? field.name;
      const allowed = GQL_FIELDS[typeName] ?? [];
      if (!allowed.includes(field.name)) {
        errors.push({ message: `Cannot query field "${field.name}" on type "${typeName}".` });
        out[key] = null;
        continue;
      }
      out[key] = this.resolveField(typeName, obj, field, variables, errors);
    }
    return out;
  }

  private resolveField(
    typeName: string,
    obj: GqlObj,
    field: GqlField,
    variables: Record<string, unknown>,
    errors: Array<{ message: string }>,
  ): unknown {
    const args = this.evalArgs(field, variables);
    const sel = field.selections;
    switch (typeName) {
      case 'Viewer':
      case 'User':
        return obj[field.name] ?? null;
      case 'Ref':
        if (field.name === 'target') {
          const target = obj.target as GqlObj | undefined;
          if (!target || !sel) return null;
          return { oid: target.oid };
        }
        return obj[field.name] ?? null;
      case 'Repository': {
        const repo = obj.repo as Repo;
        if (field.name === 'owner') return this.resolveFields('User', obj.owner as GqlObj, sel, variables, errors);
        if (field.name === 'defaultBranchRef') return this.resolveFields('Ref', obj.defaultBranchRef as GqlObj, sel, variables, errors);
        if (field.name === 'issue') {
          const issue = repo.issues.get(Number(args.number));
          if (!issue) return null;
          return this.resolveFields('Issue', this.issueObj(repo, issue), sel, variables, errors);
        }
        if (field.name === 'issues') return this.issuesConnection(repo, args, sel, variables, errors);
        if (field.name === 'pullRequests') return this.prsConnection(repo, args, sel, variables, errors);
        if (field.name === 'pullRequest') {
          const pr = repo.pulls.get(Number(args.number));
          if (!pr) return null;
          return this.resolveFields('PullRequest', this.prObj(repo, pr), sel, variables, errors);
        }
        return obj[field.name] ?? null;
      }
      case 'Issue':
        return obj[field.name] ?? null;
      case 'PullRequest': {
        const repo = obj.repo as Repo;
        const pr = obj.pr as PullRequest;
        if (field.name === 'files') return this.filesConnection(repo, pr, args, sel, variables, errors);
        if (field.name === 'headRef' || field.name === 'baseRef') {
          return this.resolveFields('Ref', obj[field.name] as GqlObj, sel, variables, errors);
        }
        if (field.name === 'author') return this.resolveFields('User', obj.author as GqlObj, sel, variables, errors);
        return obj[field.name] ?? null;
      }
      case 'IssueConnection':
      case 'PullRequestConnection':
      case 'PullRequestFileConnection':
      case 'IssueCommentConnection': {
        const conn = obj as unknown as { nodes: GqlObj[]; pageInfo: GqlObj };
        if (field.name === 'nodes') {
          return conn.nodes.map((node) => this.resolveFields(node.__type, node, sel, variables, errors));
        }
        if (field.name === 'edges') {
          return conn.nodes.map((node, i) => this.resolveFields(node.__type === 'Issue' ? 'IssueEdge' : node.__type === 'PullRequest' ? 'PullRequestEdge' : 'PullRequestFileEdge', { __type: 'edge', node, cursor: cursorOf(i) } as unknown as GqlObj, sel, variables, errors));
        }
        if (field.name === 'pageInfo') return this.resolveFields('PageInfo', conn.pageInfo, sel, variables, errors);
        if (field.name === 'totalCount') return conn.nodes.length;
        return null;
      }
      case 'IssueEdge':
      case 'PullRequestEdge':
      case 'PullRequestFileEdge':
      case 'IssueCommentEdge': {
        if (field.name === 'node') {
          const node = obj.node as GqlObj;
          return this.resolveFields(node.__type, node, sel, variables, errors);
        }
        return obj[field.name] ?? null;
      }
      case 'PullRequestFile':
        return obj[field.name] ?? null;
      case 'IssueComment':
        if (field.name === 'author') return this.resolveFields('User', obj.author as GqlObj, sel, variables, errors);
        return obj[field.name] ?? null;
      case '__Type': {
        if (field.name === 'fields') {
          const names = (obj.fields as string[] | null) ?? [];
          return names.map((n) => this.resolveFields('__Field', { __type: '__Field', name: n, description: null, args: [], type: null, isDeprecated: false, deprecationReason: null }, sel, variables, errors));
        }
        if (field.name === 'enumValues') {
          const values = (obj.enumValues as string[] | null) ?? [];
          return values.map((n) => this.resolveFields('__EnumValue', { __type: '__EnumValue', name: n, description: null, isDeprecated: false, deprecationReason: null }, sel, variables, errors));
        }
        if (field.name === 'inputFields' || field.name === 'interfaces' || field.name === 'possibleTypes') return [];
        return obj[field.name] ?? null;
      }
      case '__Field':
      case '__EnumValue':
      case '__InputValue':
        return obj[field.name] ?? null;
      case 'CreateIssuePayload':
      case 'CloseIssuePayload': {
        if (field.name === 'issue') return this.resolveFields('Issue', obj.issue as GqlObj, sel, variables, errors);
        return null;
      }
      case 'CreatePullRequestPayload':
      case 'MergePullRequestPayload': {
        if (field.name === 'pullRequest') return this.resolveFields('PullRequest', obj.pullRequest as GqlObj, sel, variables, errors);
        return null;
      }
      case 'AddCommentPayload': {
        if (field.name === 'commentEdge') return this.resolveFields('IssueCommentEdge', obj.commentEdge as GqlObj, sel, variables, errors);
        return null;
      }
      default:
        return obj[field.name] ?? null;
    }
  }

  private issuesConnection(repo: Repo, args: Record<string, unknown>, sel: GqlSelection[] | null, variables: Record<string, unknown>, errors: Array<{ message: string }>): Record<string, unknown> | null {
    let list = [...repo.issues.values()];
    const states = args.states as string[] | undefined;
    if (states && states.length) {
      const wanted = new Set(states.map((s) => String(s).toUpperCase()));
      list = list.filter((i) => wanted.has(i.state.toUpperCase()));
    }
    const orderBy = args.orderBy as { field?: string; direction?: string } | undefined;
    if (orderBy?.field === 'CREATED_AT' && orderBy.direction === 'DESC') list.sort((a, b) => b.number - a.number);
    else list.sort((a, b) => a.number - b.number);
    return this.connection(list.map((i) => this.issueObj(repo, i)), args, sel, variables, errors);
  }

  private prsConnection(repo: Repo, args: Record<string, unknown>, sel: GqlSelection[] | null, variables: Record<string, unknown>, errors: Array<{ message: string }>): Record<string, unknown> | null {
    const list = [...repo.pulls.values()].sort((a, b) => a.number - b.number);
    return this.connection(list.map((p) => this.prObj(repo, p)), args, sel, variables, errors);
  }

  private filesConnection(repo: Repo, pr: PullRequest, args: Record<string, unknown>, sel: GqlSelection[] | null, variables: Record<string, unknown>, errors: Array<{ message: string }>): Record<string, unknown> | null {
    const baseTip = repo.refs.get(`refs/heads/${pr.baseRef}`) ?? pr.baseSha;
    const headTip = repo.refs.get(`refs/heads/${pr.headRef}`) ?? pr.headSha;
    const files = diffTrees(baseTip, headTip, this.store.lookup).map((f) => ({
      __type: 'PullRequestFile',
      path: f.filename,
      additions: f.additions,
      deletions: f.deletions,
      changeType: f.status.toUpperCase(),
    }));
    return this.connection(files as unknown as GqlObj[], args, sel, variables, errors);
  }

  private connection(nodes: GqlObj[], args: Record<string, unknown>, sel: GqlSelection[] | null, variables: Record<string, unknown>, errors: Array<{ message: string }>): Record<string, unknown> | null {
    const after = indexOfCursor(typeof args.after === 'string' ? args.after : null);
    const first = typeof args.first === 'number' ? args.first : nodes.length;
    const start = after + 1;
    const page = nodes.slice(start, start + first);
    const endCursor = page.length ? cursorOf(start + page.length - 1) : null;
    const hasNextPage = start + page.length < nodes.length;
    const conn: GqlObj = {
      __type: 'IssueConnection',
      nodes: page,
      pageInfo: { __type: 'PageInfo', hasNextPage, hasPreviousPage: start > 0, startCursor: page.length ? cursorOf(start) : null, endCursor },
    };
    return this.resolveFields('IssueConnection', conn, sel, variables, errors);
  }
}

function handleGraphQL(ctx: Ctx): boolean {
  if (!authorized(ctx)) {
    sendErr(ctx, 401, 'Requires authentication');
    return true;
  }
  let body: unknown;
  try {
    body = JSON.parse(ctx.body.toString('utf8'));
  } catch {
    sendErr(ctx, 400, 'Problems parsing JSON');
    return true;
  }
  const executor = new GraphQLExecutor(ctx.store);
  const runOne = (doc: unknown): Record<string, unknown> => {
    if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) return { data: null, errors: [{ message: 'invalid document' }] };
    const d = doc as Record<string, unknown>;
    const query = typeof d.query === 'string' ? d.query : '';
    const variables = (d.variables ?? {}) as Record<string, unknown>;
    return executor.execute(query, variables);
  };
  if (Array.isArray(body)) {
    sendJson(ctx, 200, body.map(runOne));
    return true;
  }
  sendJson(ctx, 200, runOne(body));
  return true;
}

// -------------------------------------------------------------------------------------------
// MCP (JSON-RPC 2.0 over Streamable HTTP; plain JSON answers, no SSE)
// -------------------------------------------------------------------------------------------

interface McpToolDef {
  name: string;
  description: string;
  inputSchema: { type: 'object'; properties: Record<string, Record<string, unknown>>; required: string[] };
}

const MCP_TOOLS: McpToolDef[] = [
  {
    name: 'create_repo',
    description: 'Create a repository owned by the authenticated user.',
    inputSchema: {
      type: 'object',
      properties: {
        name: { type: 'string', description: 'Repository name' },
        description: { type: 'string', description: 'Repository description' },
        private: { type: 'boolean', description: 'Whether the repository is private' },
      },
      required: ['name'],
    },
  },
  {
    name: 'get_repo',
    description: 'Get a repository by owner and name.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login' },
        repo: { type: 'string', description: 'Repository name' },
        full_name: { type: 'string', description: 'owner/repo' },
      },
      required: ['owner', 'repo'],
    },
  },
  {
    name: 'create_issue',
    description: 'Create an issue in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login' },
        repo: { type: 'string', description: 'Repository name' },
        full_name: { type: 'string', description: 'owner/repo' },
        title: { type: 'string', description: 'Issue title' },
        body: { type: 'string', description: 'Issue body' },
      },
      required: ['owner', 'repo', 'title'],
    },
  },
  {
    name: 'list_issues',
    description: 'List issues in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login' },
        repo: { type: 'string', description: 'Repository name' },
        full_name: { type: 'string', description: 'owner/repo' },
        state: { type: 'string', description: 'open, closed or all' },
      },
      required: ['owner', 'repo'],
    },
  },
  {
    name: 'create_pull_request',
    description: 'Create a pull request in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login' },
        repo: { type: 'string', description: 'Repository name' },
        full_name: { type: 'string', description: 'owner/repo' },
        title: { type: 'string', description: 'Pull request title' },
        body: { type: 'string', description: 'Pull request body' },
        head: { type: 'string', description: 'Head branch name' },
        base: { type: 'string', description: 'Base branch name' },
      },
      required: ['owner', 'repo', 'title', 'head', 'base'],
    },
  },
  {
    name: 'merge_pull_request',
    description: 'Merge a pull request with a squash merge.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login' },
        repo: { type: 'string', description: 'Repository name' },
        full_name: { type: 'string', description: 'owner/repo' },
        pull_number: { type: 'integer', description: 'Pull request number' },
      },
      required: ['owner', 'repo', 'pull_number'],
    },
  },
  {
    name: 'list_pull_requests',
    description: 'List pull requests in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login' },
        repo: { type: 'string', description: 'Repository name' },
        full_name: { type: 'string', description: 'owner/repo' },
        state: { type: 'string', description: 'open, closed or all' },
      },
      required: ['owner', 'repo'],
    },
  },
];

function mcpRepoArgs(args: Record<string, unknown>): { owner: string; repo: string } {
  let owner = typeof args.owner === 'string' ? args.owner : '';
  let repo = typeof args.repo === 'string' ? args.repo : '';
  if ((owner === '' || repo === '') && typeof args.full_name === 'string') {
    const slash = args.full_name.indexOf('/');
    if (slash > 0) {
      owner = args.full_name.slice(0, slash);
      repo = args.full_name.slice(slash + 1);
    }
  }
  return { owner, repo };
}

function mcpToolResult(value: unknown): Record<string, unknown> {
  const text = JSON.stringify(value);
  return { content: [{ type: 'text', text }], structuredContent: value };
}

function mcpToolError(message: string): Record<string, unknown> {
  return { content: [{ type: 'text', text: message }], isError: true };
}

function runMcpTool(store: Store, name: string, args: Record<string, unknown>): Record<string, unknown> {
  const tool = MCP_TOOLS.find((t) => t.name === name);
  if (!tool) return mcpToolError(`unknown tool ${name}`);
  for (const required of tool.inputSchema.required) {
    if (args[required] === undefined || args[required] === null || args[required] === '') {
      // A full_name can stand in for owner+repo.
      if ((required === 'owner' || required === 'repo') && typeof args.full_name === 'string' && args.full_name.includes('/')) continue;
      return mcpToolError(`missing required argument: ${required}`);
    }
  }
  if (name === 'create_repo') {
    const repoName = String(args.name);
    if (!NAME_RE.test(repoName) || repoName.endsWith('.git')) return mcpToolError('invalid repository name');
    if (store.getRepo(VIEWER_LOGIN, repoName)) return mcpToolError('name already exists on this account');
    const repo = store.createRepo(VIEWER_LOGIN, repoName, args.private === true, typeof args.description === 'string' ? args.description : '');
    return mcpToolResult(repoJson(repo));
  }
  const { owner, repo: repoName } = mcpRepoArgs(args);
  const repo = owner === VIEWER_LOGIN ? store.getRepo(owner, repoName) : undefined;
  if (!repo) return mcpToolError('repository not found');
  if (name === 'get_repo') return mcpToolResult(repoJson(repo));
  if (name === 'create_issue') {
    const issue = store.createIssue(repo, String(args.title), typeof args.body === 'string' ? args.body : '', []);
    return mcpToolResult(issueJson(repo, issue));
  }
  if (name === 'list_issues') {
    const state = typeof args.state === 'string' ? args.state : 'open';
    let list = [...repo.issues.values()];
    if (state !== 'all') list = list.filter((i) => i.state === state);
    list.sort((a, b) => a.number - b.number);
    return mcpToolResult(list.map((i) => issueJson(repo, i)));
  }
  if (name === 'create_pull_request') {
    const head = String(args.head);
    const base = String(args.base);
    if (!repo.refs.has(`refs/heads/${head}`) || !repo.refs.has(`refs/heads/${base}`)) {
      return mcpToolError('head or base branch does not exist');
    }
    const pr = store.createPullRequest(repo, String(args.title), typeof args.body === 'string' ? args.body : '', head, base);
    if (!pr) return mcpToolError('could not create pull request');
    return mcpToolResult(prJson(store, repo, pr));
  }
  if (name === 'merge_pull_request') {
    const number = Number(args.pull_number);
    const pr = repo.pulls.get(number);
    if (!pr) return mcpToolError('pull request not found');
    const merged = mergePullRequest(store, repo, pr);
    if (merged === 'already') return mcpToolError('pull request is already merged');
    if (merged === 'conflict') return mcpToolError('merge conflict');
    if (merged === 'not-found') return mcpToolError('pull request not found');
    return mcpToolResult({ merged: true, sha: merged, message: 'Pull request successfully merged' });
  }
  if (name === 'list_pull_requests') {
    const state = typeof args.state === 'string' ? args.state : 'open';
    let list = [...repo.pulls.values()];
    if (state !== 'all') list = list.filter((p) => p.state === state);
    list.sort((a, b) => a.number - b.number);
    return mcpToolResult(list.map((p) => prJson(store, repo, p)));
  }
  return mcpToolError(`unknown tool ${name}`);
}

function handleMcp(ctx: Ctx): boolean {
  if (ctx.method === 'DELETE') {
    ctx.res.writeHead(200, { 'x-github-request-id': ctx.store.nextReqId() });
    ctx.res.end();
    return true;
  }
  if (ctx.method !== 'POST') {
    sendErr(ctx, 404, 'Not Found');
    return true;
  }
  if (!authorized(ctx)) {
    sendErr(ctx, 401, 'Requires authentication');
    return true;
  }
  let body: unknown;
  try {
    body = JSON.parse(ctx.body.toString('utf8'));
  } catch {
    ctx.res.writeHead(400, { 'content-type': 'application/json', 'x-github-request-id': ctx.store.nextReqId() });
    ctx.res.end(JSON.stringify({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error' } }));
    return true;
  }
  if (body === null || typeof body !== 'object' || Array.isArray(body)) {
    ctx.res.writeHead(400, { 'content-type': 'application/json', 'x-github-request-id': ctx.store.nextReqId() });
    ctx.res.end(JSON.stringify({ jsonrpc: '2.0', id: null, error: { code: -32600, message: 'Invalid Request' } }));
    return true;
  }
  const rpc = body as Record<string, unknown>;
  const id = rpc.id;
  const method = typeof rpc.method === 'string' ? rpc.method : '';
  // A notification (no id) gets a 202 with no body.
  if (id === undefined) {
    ctx.res.writeHead(202, { 'x-github-request-id': ctx.store.nextReqId() });
    ctx.res.end();
    return true;
  }
  const params = (rpc.params ?? {}) as Record<string, unknown>;
  const reply = (value: Record<string, unknown>): void => {
    ctx.res.writeHead(200, { 'content-type': 'application/json', 'x-github-request-id': ctx.store.nextReqId() });
    ctx.res.end(JSON.stringify({ jsonrpc: '2.0', id, ...value }));
  };
  if (method === 'server/discover') {
    reply({
      result: {
        supportedVersions: ['2026-07-28', '2025-11-25'],
        protocolVersion: '2026-07-28',
        capabilities: { tools: {} },
        serverInfo: { name: SERVER_NAME, version: SERVER_VERSION },
        _meta: { 'io.modelcontextprotocol/serverInfo': { name: SERVER_NAME, version: SERVER_VERSION } },
      },
    });
    return true;
  }
  if (method === 'initialize') {
    const requested = typeof params.protocolVersion === 'string' ? params.protocolVersion : '';
    const chosen = requested === '2026-07-28' || requested === '2025-11-25' || requested === '2025-06-18' || requested === '2025-03-26' || requested === '2024-11-05' ? requested : '2026-07-28';
    reply({
      result: {
        protocolVersion: chosen,
        capabilities: { tools: {} },
        serverInfo: { name: SERVER_NAME, version: SERVER_VERSION },
      },
    });
    return true;
  }
  if (method === 'tools/list') {
    reply({ result: { tools: MCP_TOOLS } });
    return true;
  }
  if (method === 'tools/call') {
    const toolName = typeof params.name === 'string' ? params.name : '';
    const args = (params.arguments ?? {}) as Record<string, unknown>;
    const tool = MCP_TOOLS.find((t) => t.name === toolName);
    if (!tool) {
      reply({ error: { code: -32601, message: `unknown tool ${toolName}` } });
      return true;
    }
    for (const required of tool.inputSchema.required) {
      if (args[required] === undefined || args[required] === null || args[required] === '') {
        if ((required === 'owner' || required === 'repo') && typeof args.full_name === 'string' && args.full_name.includes('/')) continue;
        reply({ error: { code: -32602, message: `missing required argument: ${required}` } });
        return true;
      }
    }
    reply({ result: runMcpTool(ctx.store, toolName, args) });
    return true;
  }
  reply({ error: { code: -32601, message: `method not found: ${method}` } });
  return true;
}

// -------------------------------------------------------------------------------------------
// server
// -------------------------------------------------------------------------------------------

function readBody(req: IncomingMessage): Promise<Buffer> {
  return new Promise<Buffer>((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on('data', (chunk: Buffer) => {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

async function handleRequest(store: Store, token: string, req: IncomingMessage, res: ServerResponse): Promise<void> {
  try {
    const body = req.method === 'GET' || req.method === 'HEAD' ? Buffer.alloc(0) : await readBody(req);
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    const path = decodeURIComponent(url.pathname);
    const ctx: Ctx = {
      store,
      req,
      res,
      method: req.method ?? 'GET',
      path,
      query: url.searchParams,
      body,
      token,
      origin: `http://${req.headers.host ?? '127.0.0.1'}`,
    };
    if (ctx.method === 'GET' && path === '/healthz') {
      sendJson(ctx, 200, { ok: true });
      return;
    }
    if (path === '/api/graphql' && ctx.method === 'POST') {
      handleGraphQL(ctx);
      return;
    }
    if (path === '/mcp') {
      handleMcp(ctx);
      return;
    }
    if (path.startsWith('/api/v3')) {
      handleRest(ctx);
      return;
    }
    if (handleGit(ctx)) return;
    sendErr(ctx, 404, 'Not Found');
  } catch (err) {
    try {
      res.writeHead(500, { 'content-type': 'application/json', 'x-github-request-id': store.nextReqId() });
      res.end(JSON.stringify({ message: err instanceof Error ? err.message : 'Internal error' }));
    } catch {
      // The socket is already gone; nothing more to do.
    }
  }
}

export function createApp(): Server {
  const store = new Store();
  const token = process.env.MAKEFUN_SHOP_TOKEN ?? '';
  const server = createServer((req, res) => {
    void handleRequest(store, token, req, res);
  });
  return server;
}
