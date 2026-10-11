function matchShadowSegment(root, spec, gather, limit = 2) {
  if (spec.selector) {
    const nodes = root.querySelectorAll(spec.selector);
    if (nodes.length > 10000) return 'too-many';
    return Array.from(nodes).slice(0, limit);
  }
  const result = gather(root, { maxNodes: 10000, pierceOpen: false, includeElements: true });
  if (result.truncated) return 'too-many';
  const norm = value => String(value ?? '').replace(/\s+/g, ' ').trim();
  return result.candidates.filter(candidate => candidate.role === spec.role &&
    norm(candidate.name) === norm(spec.name)).slice(0, limit).map(candidate => candidate.element);
}
