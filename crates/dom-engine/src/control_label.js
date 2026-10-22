export function controlLabel(control, consume = () => true) {
  const labels = control.labels?.length ? control.labels : [control.closest('label')].filter(Boolean);
  let remaining = 4096;
  let characters = 2048;
  let exhausted = false;
  const text = value => {
    characters -= value.length;
    if (characters < 0) exhausted = true;
    return exhausted ? '' : value;
  };
  const read = (node, depth = 0) => {
    if (exhausted) return '';
    if (--remaining < 0 || depth > 128 || !consume()) {
      exhausted = true;
      return '';
    }
    if (node === control) return '';
    if (node.nodeType === 3) return text(node.nodeValue || '');
    if (node.nodeType !== 1) return '';
    const tag = node.localName;
    if (tag === 'script' || tag === 'style' || node.getAttribute('aria-hidden') === 'true') return '';
    const style = node.ownerDocument.defaultView.getComputedStyle(node);
    if (style.display === 'none' || style.visibility === 'hidden') return '';
    if (tag === 'br') return ' ';
    if (tag === 'img') return text(node.getAttribute('alt') || '');
    let content = '';
    for (const child of node.childNodes) {
      content += read(child, depth + 1);
      if (exhausted) break;
    }
    return /^(block|flex|grid|table|list-item|flow-root)/.test(style.display) ? ' ' + content + ' ' : content;
  };
  let result = '';
  for (const label of labels) {
    result += ' ' + read(label);
    if (exhausted) return null;
  }
  return result.replace(/\s+/g, ' ').trim() || null;
}
