function gatherCandidates(root, options = {}) {
  const controlLabel = __CONTROL_LABEL_FUNCTION__;
  const prefix = options.prefix || '';
  const maxNodes = options.maxNodes ?? Infinity;
  const maxCandidates = options.maxCandidates ?? Infinity;
  const publicMetadata = options.redact === true;
  const selector = options.selector || (options.controlsOnly ? 'input,textarea,select,button,a[href],iframe,[role]' : '*');
  let n=0, visited=0, truncated=false;
  const candidates=[], stamps=new Set();
  const labelledBy = element => {
    const root = element.getRootNode();
    return (element.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
      .map(id => String(root.getElementById?.(id)?.textContent || '').trim())
      .filter(Boolean).join(' ') || null;
  };
const contentName=el=>{const parts=[];const walk=n=>{if(n.nodeType===3){parts.push(n.nodeValue);return;}if(n.nodeType!==1)return;const tag=n.tagName.toLowerCase();if(tag==='script'||tag==='style')return;if(tag==='img'){parts.push(' '+(n.getAttribute('alt')||'')+' ');return;}if(tag==='title'&&n.parentElement instanceof SVGElement){parts.push(' '+n.textContent+' ');return;}const s=getComputedStyle(n);if(s.display==='none'||s.visibility==='hidden')return;for(const c of n.childNodes)walk(c);};walk(el);return parts.join('').replace(/\s+/g,' ').trim()||null;};
const implicitRole=el=>{if(el.tagName==='BUTTON')return 'button';if(el.tagName==='A'&&el.hasAttribute('href'))return 'link';if(el.tagName==='IFRAME')return 'iframe';if(el.tagName==='TEXTAREA'||el.isContentEditable)return 'textbox';if(el.tagName==='SELECT')return el.multiple?'listbox':'combobox';if(el.tagName==='FORM')return 'form';if(el.tagName==='DIALOG')return 'dialog';if(el.tagName==='MAIN')return 'main';if(el.tagName==='NAV')return 'navigation';if(el.tagName==='ARTICLE')return 'article';if(el.tagName==='SECTION'&&(el.getAttribute('aria-label')||el.getAttribute('aria-labelledby')))return 'region';if(/^H[1-6]$/.test(el.tagName))return 'heading';if(el.tagName==='UL'||el.tagName==='OL'||el.tagName==='MENU'||el.tagName==='DL')return 'list';if(el.tagName==='LI')return 'listitem';if(el.tagName==='IMG')return 'img';if(el.tagName==='OPTION')return 'option';if(el.tagName==='TABLE')return 'table';if(el.tagName==='TR')return 'row';if(el.tagName==='TD')return 'cell';if(el.tagName==='TH')return 'columnheader';if(el.tagName==='P')return 'paragraph';if(el.tagName==='HR')return 'separator';if(el.tagName==='FIELDSET'||el.tagName==='DETAILS')return 'group';if(el.tagName==='ASIDE')return 'complementary';if(el.tagName==='HEADER')return 'banner';if(el.tagName==='FOOTER')return 'contentinfo';if(el.tagName==='PROGRESS')return 'progressbar';if(el.tagName==='METER')return 'meter';if(el.tagName==='OUTPUT')return 'status';if(el.tagName==='SUMMARY')return 'button';if(el.tagName==='FIGURE')return 'figure';if(el.tagName==='BLOCKQUOTE')return 'blockquote';if(el.tagName==='DT')return 'term';if(el.tagName==='DD')return 'definition';if(el.tagName==='AREA'&&el.hasAttribute('href'))return 'link';if(el.tagName==='TIME')return 'time';if(el.tagName!=='INPUT')return null;const type=(el.type||'text').toLowerCase();if(['button','submit','reset','image','file'].includes(type))return 'button';if(type==='checkbox')return 'checkbox';if(type==='radio')return 'radio';if(type==='range')return 'slider';if(type==='number')return 'spinbutton';if(type==='search')return 'searchbox';return type==='hidden'?null:'textbox'};
  const bounded=value=>typeof value==='string'?value.trim().replace(/\s+/g,' ').slice(0,256):'';
  const secret=/(?:authorization|authentication|bearer|token|secret|password|passwd|api[-_]?key|credential)/i;
  const paymentValue=/(?:card(?: number)?|cvc|cvv|otp|expiry|challenge code)\s*[:=]?\s*\d{3,}/i;
  const credentialShape=/(?:\d[ -]?){12,19}|[a-zA-Z0-9_-]{32,}/;
  const publicLabels=new Set(['Password','Authentication code','Card details','Card number','Expiry','CVC','3-D Secure challenge','Challenge code']);
  const isCredentialShaped=token=>/\d/.test(token)||token.endsWith('=')||(/[A-Z]/.test(token.slice(1))&&/[a-z]/.test(token));
  const hasAuthScheme=value=>Array.from(value.matchAll(/(?:^|\s)(?:bearer|basic)\s+([A-Za-z0-9._~+\/=-]{8,})/gi)).some(match=>isCredentialShaped(match[1]));
  const hasLongRun=value=>Array.from(value.matchAll(/[A-Za-z0-9+\/_=-]{40,}/g)).some(match=>/\d/.test(match[0])&&/[A-Z]/.test(match[0])&&/[a-z]/.test(match[0]));
  const secretMaterial=value=>hasAuthScheme(value)||/(?:password|passwd|secret|token|api[-_ ]?key|credentials?|authorization|private[-_ ]?(?:key|token|secret)|pairing[-_ ]?code)\s*[:=]\s*\S{4}/i.test(value)||/-----BEGIN[\s\S]*?PRIVATE KEY-----/.test(value)||/eyJ[\w-]{7,}\.[\w-]{10,}\.[\w-]{10,}/.test(value)||/(?<![A-Za-z0-9])(?:sk-|sk_live_|sk_test_|rk_live_|ghp_|gho_|ghu_|ghs_|github_pat_|xox[abprs]-)[A-Za-z0-9_-]{16,}/.test(value)||/(?<![A-Z0-9])AKIA[A-Z0-9]{16}/.test(value)||/AIza[A-Za-z0-9_-]{35}/.test(value)||(!/^https?:\/\//i.test(value.trim())&&hasLongRun(value));
  const safeLabel=value=>publicLabels.has(value)?value:(secretMaterial(value)||paymentValue.test(value)||credentialShape.test(value)?'[redacted]':value);
  const sensitive=element=>['input','textarea','select'].includes(element.localName)&&((element.localName==='input'&&element.type==='password')||Array.from(element.attributes).some(attribute=>secret.test(attribute.name)||secret.test(attribute.value)||paymentValue.test(attribute.value)||/(?:card|cvc|cvv|otp|one[-_]?time|expiry|challenge|code)/i.test(attribute.name)||credentialShape.test(attribute.value)));
  const path=element=>{
    const parts=[];
    for(let current=element;current;current=current.parentElement){
      if(parts.length>=12)return null;
      const tag=current.localName;
      if(!/^[a-z][a-z0-9-]*$/.test(tag))return null;
      let position=1;
      for(let peer=current.previousElementSibling;peer;peer=peer.previousElementSibling){
        if(peer.localName===tag)position++;
      }
      parts.unshift(`${tag}:nth-of-type(${position})`);
    }
    const selector=parts.join(' > ');
    if(selector.length>2048)return null;
    const parentRoot=element.getRootNode();
    if(parentRoot.host){
      const hostPath=path(parentRoot.host);
      return hostPath&&hostPath.length+selector.length+5<=2048?hostPath+' >>> '+selector:null;
    }
    return selector;
  };

  const visit = current => {
    for(const element of current.querySelectorAll('*')){
      if(++visited>maxNodes){truncated=true;return;}
      if(options.controlsOnly&&!element.matches(selector)){
        if(options.pierceOpen!==false&&element.shadowRoot)visit(element.shadowRoot);
        if(truncated)return;
        continue;
      }
      // An element keeps the id an earlier scan stamped, so concurrent scans agree;
      // a copy of a stamped element takes a fresh one.
      const stamped=prefix?element.getAttribute('data-bobby-target'):null;
      const id=stamped&&/^bobby-\d+-\d+$/.test(stamped)&&!stamps.has(stamped)?stamped:prefix+(++n);
      if(prefix){stamps.add(id);if(id!==stamped)element.setAttribute('data-bobby-target',id);}
      const style=getComputedStyle(element), rect=element.getBoundingClientRect();
      const visible=style.visibility!=='hidden'&&style.display!=='none'&&rect.width>0&&rect.height>0;
      const tag=element.localName;
      const role=element.getAttribute('role')||implicitRole(element);
      const rawLabel=element.labels?.length ? controlLabel(element) : null;
      const rawName=labelledBy(element)||element.getAttribute('aria-label')||rawLabel||
        (tag==='img'||(tag==='input'&&element.type==='image')?element.getAttribute('alt'):null)||(tag==='iframe'?element.getAttribute('title'):null)||
        (tag==='input'&&['button','submit','reset'].includes(element.type)?element.value:null)||
        (tag==='input'||tag==='textarea'?element.getAttribute('placeholder'):null)||
        String(element.innerText||'').trim()||contentName(element)||element.getAttribute('title')||null;
      const isSensitive=publicMetadata&&sensitive(element);
      const expose=value=>bounded(isSensitive?(publicLabels.has(value)?value:'[redacted]'):safeLabel(value||''));
      const name=publicMetadata?expose(rawName):rawName;
      const label=publicMetadata?expose(rawLabel):rawLabel;
      const attributes={};
      if(!publicMetadata){
        for(const attribute of element.attributes)
          if(['id','name','type','src','href','placeholder','autocomplete','pattern','min','max','step','multiple','aria-invalid','aria-label','title'].includes(attribute.name)||attribute.name.startsWith('data-'))
            attributes[attribute.name]=attribute.value;
        for(const booleanName of ['required','readonly','checked','multiple'])
          if(element[booleanName]===true)attributes[booleanName]='true';
      }
      // An input's type is a fixed vocabulary, kept even when page metadata is
      // redacted and normalized the way the accessibility tree reports it.
      if(element.localName==='input')attributes.type=String(element.type||'text').toLowerCase();
      const aria=element.getAttribute('aria-label');
      const uniqueId=Boolean(element.id)&&element.getRootNode().querySelector('#'+CSS.escape(element.id))===element;
      const css=publicMetadata?path(element):uniqueId?`#${CSS.escape(element.id)}`:aria?`[aria-label="${aria.replace(/\\/g,'\\\\').replace(/"/g,'\\"')}"]`:tag;
      if((!options.controlsOnly||visible)&&css){
        candidates.push({id,css,tag,testId:publicMetadata?null:element.getAttribute('data-testid'),role,name,label,
          text:publicMetadata?name:String(element.innerText||element.value||'').trim(),attributes,
          attached:element.isConnected,visible,enabled:!element.disabled&&element.getAttribute('aria-disabled')!=='true'&&!element.closest('fieldset[disabled]'),
          ...(options.includeElements?{element}:{})});
        if(candidates.length>maxCandidates){truncated=true;return;}
      }
      if(options.pierceOpen!==false&&element.shadowRoot)visit(element.shadowRoot);
      if(truncated)return;
    }
  };
  visit(root);
  return {truncated,candidates:truncated?[]:candidates};
}
