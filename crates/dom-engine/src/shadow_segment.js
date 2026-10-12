function matchShadowSegment(root, spec, gather, limit = 2) {
  const select = matches => {
    if (spec.ordinal == null) return matches.slice(0, limit);
    if (!Number.isSafeInteger(spec.ordinal) || spec.ordinal < 0 || spec.ordinal >= matches.length) return [];
    return [matches[spec.ordinal]];
  };
  if (spec.selector) {
    const nodes = root.querySelectorAll(spec.selector);
    if (nodes.length > 10000) return 'too-many';
    return select(Array.from(nodes));
  }
  const result = gather(root, { maxNodes: 10000, pierceOpen: false, includeElements: true });
  if (result.truncated) return 'too-many';
  const norm = value => String(value ?? '').replace(/\s+/g, ' ').trim();
  return select(result.candidates.filter(candidate => candidate.role === spec.role &&
    norm(candidate.name) === norm(spec.name))).map(candidate => candidate.element);
}
