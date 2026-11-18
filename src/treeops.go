package main

// Tree path operations, diff and merge helpers — ported from the Node forge
// with identical semantics.

import (
	"sort"
	"strings"
)

type lookupFunc func(oid string) *gitObject

// flattenTree flattens a tree to path -> {oid, mode} (files only; dirs recursed).
func flattenTree(rootOID string, lookup lookupFunc) map[string]struct {
	oid  string
	mode string
} {
	out := make(map[string]struct {
		oid  string
		mode string
	})
	if rootOID == "" {
		return out
	}
	var walk func(treeOID, prefix string)
	walk = func(treeOID, prefix string) {
		obj := lookup(treeOID)
		if obj == nil || obj.typ != objTree {
			return
		}
		entries, err := parseTree(obj.data)
		if err != nil {
			return
		}
		for _, e := range entries {
			path := e.name
			if prefix != "" {
				path = prefix + "/" + e.name
			}
			if e.mode == "40000" {
				walk(e.oid, path)
			} else {
				out[path] = struct {
					oid  string
					mode string
				}{e.oid, e.mode}
			}
		}
	}
	walk(rootOID, "")
	return out
}

// setPath sets (or replaces) the blob at segments in the tree rooted at rootOID
// ("" = empty tree). Returns the new root tree oid.
func setPath(rootOID string, segments []string, blobOID string, lookup lookupFunc, put func(gitObjectType, []byte) string) string {
	var rec func(treeOID string, depth int) string
	rec = func(treeOID string, depth int) string {
		var entries []treeEntry
		if treeOID != "" {
			if obj := lookup(treeOID); obj != nil {
				entries, _ = parseTree(obj.data)
			}
		}
		name := segments[depth]
		if depth == len(segments)-1 {
			idx := -1
			for i, e := range entries {
				if e.name == name {
					idx = i
					break
				}
			}
			if idx >= 0 {
				entries[idx] = treeEntry{mode: "100644", name: name, oid: blobOID}
			} else {
				entries = append(entries, treeEntry{mode: "100644", name: name, oid: blobOID})
			}
			return put(objTree, buildTree(entries))
		}
		idx := -1
		for i, e := range entries {
			if e.name == name {
				idx = i
				break
			}
		}
		var childOID string
		if idx >= 0 && entries[idx].mode == "40000" {
			childOID = entries[idx].oid
		}
		newChild := rec(childOID, depth+1)
		if idx >= 0 {
			entries[idx] = treeEntry{mode: "40000", name: name, oid: newChild}
		} else {
			entries = append(entries, treeEntry{mode: "40000", name: name, oid: newChild})
		}
		return put(objTree, buildTree(entries))
	}
	return rec(rootOID, 0)
}

// deletePath deletes the path at segments from the tree; returns the new root.
func deletePath(rootOID string, segments []string, lookup lookupFunc, put func(gitObjectType, []byte) string) string {
	if rootOID == "" {
		return emptyTreeOID
	}
	var rec func(treeOID string, depth int) string
	rec = func(treeOID string, depth int) string {
		var entries []treeEntry
		if obj := lookup(treeOID); obj != nil {
			entries, _ = parseTree(obj.data)
		}
		name := segments[depth]
		idx := -1
		for i, e := range entries {
			if e.name == name {
				idx = i
				break
			}
		}
		if idx < 0 {
			return treeOID
		}
		if depth == len(segments)-1 {
			entries = append(entries[:idx], entries[idx+1:]...)
			return put(objTree, buildTree(entries))
		}
		if entries[idx].mode != "40000" {
			return treeOID
		}
		newChild := rec(entries[idx].oid, depth+1)
		if newChild == entries[idx].oid {
			return treeOID
		}
		var childEntries []treeEntry
		if obj := lookup(newChild); obj != nil {
			childEntries, _ = parseTree(obj.data)
		}
		if len(childEntries) == 0 {
			entries = append(entries[:idx], entries[idx+1:]...)
		} else {
			entries[idx] = treeEntry{mode: "40000", name: name, oid: newChild}
		}
		return put(objTree, buildTree(entries))
	}
	return rec(rootOID, 0)
}

// getPath returns the blob oid at segments, or "" when missing or a directory.
func getPath(rootOID string, segments []string, lookup lookupFunc) string {
	if rootOID == "" {
		return ""
	}
	treeOID := rootOID
	for depth, seg := range segments {
		obj := lookup(treeOID)
		if obj == nil || obj.typ != objTree {
			return ""
		}
		entries, err := parseTree(obj.data)
		if err != nil {
			return ""
		}
		var found *treeEntry
		for i := range entries {
			if entries[i].name == seg {
				found = &entries[i]
				break
			}
		}
		if found == nil {
			return ""
		}
		if depth == len(segments)-1 {
			if found.mode == "40000" {
				return ""
			}
			return found.oid
		}
		if found.mode != "40000" {
			return ""
		}
		treeOID = found.oid
	}
	return ""
}

// listDir returns the entries of the directory at segments (empty = root), or
// nil when the path is not a directory.
func listDir(rootOID string, segments []string, lookup lookupFunc) []treeEntry {
	if rootOID == "" {
		if len(segments) == 0 {
			return []treeEntry{}
		}
		return nil
	}
	treeOID := rootOID
	for _, seg := range segments {
		obj := lookup(treeOID)
		if obj == nil || obj.typ != objTree {
			return nil
		}
		entries, err := parseTree(obj.data)
		if err != nil {
			return nil
		}
		var found *treeEntry
		for i := range entries {
			if entries[i].name == seg {
				found = &entries[i]
				break
			}
		}
		if found == nil || found.mode != "40000" {
			return nil
		}
		treeOID = found.oid
	}
	obj := lookup(treeOID)
	if obj == nil || obj.typ != objTree {
		return nil
	}
	entries, err := parseTree(obj.data)
	if err != nil {
		return nil
	}
	return entries
}

// isDescendant reports whether ancestor is reachable from tip.
func isDescendant(tip, ancestor string, lookup lookupFunc) bool {
	if tip == ancestor {
		return true
	}
	// Fast path: the overwhelmingly common case is a single-commit push,
	// whose parent IS the ancestor. Answer it with one lookup and no map
	// or queue; fall through to the BFS only when it is not a direct parent.
	if obj := lookup(tip); obj != nil && obj.typ == objCommit {
		for _, p := range parseCommit(obj.data).parents {
			if p == ancestor {
				return true
			}
		}
	}
	seen := make(map[string]bool)
	queue := []string{tip}
	for len(queue) > 0 {
		oid := queue[len(queue)-1]
		queue = queue[:len(queue)-1]
		if seen[oid] {
			continue
		}
		seen[oid] = true
		obj := lookup(oid)
		if obj == nil || obj.typ != objCommit {
			continue
		}
		for _, p := range parseCommit(obj.data).parents {
			if p == ancestor {
				return true
			}
			queue = append(queue, p)
		}
	}
	return false
}

// mergeBase returns the first common ancestor of two commits, or "".
func mergeBase(a, b string, lookup lookupFunc) string {
	ancestors := make(map[string]bool)
	queueA := []string{a}
	for len(queueA) > 0 {
		oid := queueA[len(queueA)-1]
		queueA = queueA[:len(queueA)-1]
		if ancestors[oid] {
			continue
		}
		ancestors[oid] = true
		if obj := lookup(oid); obj != nil && obj.typ == objCommit {
			queueA = append(queueA, parseCommit(obj.data).parents...)
		}
	}
	queueB := []string{b}
	seenB := make(map[string]bool)
	for len(queueB) > 0 {
		oid := queueB[len(queueB)-1]
		queueB = queueB[:len(queueB)-1]
		if seenB[oid] {
			continue
		}
		seenB[oid] = true
		if ancestors[oid] {
			return oid
		}
		if obj := lookup(oid); obj != nil && obj.typ == objCommit {
			queueB = append(queueB, parseCommit(obj.data).parents...)
		}
	}
	return ""
}

type fileChange struct {
	filename  string
	status    string // added | modified | removed
	additions int
	deletions int
	changes   int
	oldOID    string
	newOID    string
}

// lineDiff is the multiset line difference: [added, removed].
func lineDiff(oldText, newText string) (int, int) {
	count := func(text string) map[string]int {
		m := make(map[string]int)
		if text == "" {
			return m
		}
		for _, line := range strings.Split(text, "\n") {
			m[line]++
		}
		return m
	}
	a := count(oldText)
	b := count(newText)
	added, removed := 0, 0
	for line, n := range b {
		if d := n - a[line]; d > 0 {
			added += d
		}
	}
	for line, n := range a {
		if d := n - b[line]; d > 0 {
			removed += d
		}
	}
	return added, removed
}

// diffTrees diffs base tip tree -> head tip tree, with line counts.
func diffTrees(baseTip, headTip string, lookup lookupFunc) []fileChange {
	var baseTree, headTree string
	if baseTip != "" {
		if obj := lookup(baseTip); obj != nil {
			baseTree = parseCommit(obj.data).tree
		}
	}
	if headTip != "" {
		if obj := lookup(headTip); obj != nil {
			headTree = parseCommit(obj.data).tree
		}
	}
	baseFlat := flattenTree(baseTree, lookup)
	headFlat := flattenTree(headTree, lookup)
	paths := make(map[string]bool)
	for p := range baseFlat {
		paths[p] = true
	}
	for p := range headFlat {
		paths[p] = true
	}
	sorted := make([]string, 0, len(paths))
	for p := range paths {
		sorted = append(sorted, p)
	}
	sort.Strings(sorted)
	var out []fileChange
	for _, path := range sorted {
		oldEntry, hasOld := baseFlat[path]
		newEntry, hasNew := headFlat[path]
		if hasOld && hasNew && oldEntry.oid == newEntry.oid {
			continue
		}
		var oldText, newText string
		if hasOld {
			if obj := lookup(oldEntry.oid); obj != nil {
				oldText = string(obj.data)
			}
		}
		if hasNew {
			if obj := lookup(newEntry.oid); obj != nil {
				newText = string(obj.data)
			}
		}
		additions, deletions := lineDiff(oldText, newText)
		status := "modified"
		if !hasOld {
			status = "added"
		} else if !hasNew {
			status = "removed"
		}
		fc := fileChange{filename: path, status: status, additions: additions, deletions: deletions, changes: additions + deletions}
		if hasOld {
			fc.oldOID = oldEntry.oid
		}
		if hasNew {
			fc.newOID = newEntry.oid
		}
		out = append(out, fc)
	}
	return out
}

// mergeConflicts reports whether merging head into base would conflict.
func mergeConflicts(baseTip, headTip string, lookup lookupFunc) bool {
	base := mergeBase(baseTip, headTip, lookup)
	if base == "" {
		return false
	}
	treeOf := func(tip string) string {
		if obj := lookup(tip); obj != nil {
			return parseCommit(obj.data).tree
		}
		return ""
	}
	baseFlat := flattenTree(treeOf(base), lookup)
	baseTipFlat := flattenTree(treeOf(baseTip), lookup)
	headTipFlat := flattenTree(treeOf(headTip), lookup)
	paths := make(map[string]bool)
	for p := range baseFlat {
		paths[p] = true
	}
	for p := range baseTipFlat {
		paths[p] = true
	}
	for p := range headTipFlat {
		paths[p] = true
	}
	for path := range paths {
		m := ""
		if e, ok := baseFlat[path]; ok {
			m = e.oid
		}
		b := ""
		if e, ok := baseTipFlat[path]; ok {
			b = e.oid
		}
		h := ""
		if e, ok := headTipFlat[path]; ok {
			h = e.oid
		}
		if b != m && h != m && b != h {
			return true
		}
	}
	return false
}
