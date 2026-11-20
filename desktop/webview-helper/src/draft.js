// Only the top document on the service's exact origin receives the nonce.
(origin, nonce) => {
  if (self !== top || location.origin !== origin) return;
  let pending;
  const post = window.webkit.messageHandlers.muqunDraft.postMessage.bind(window.webkit.messageHandlers.muqunDraft);
  Object.defineProperty(window, "__muqunAppendToDraft", { value(text) {
    if (!navigator.userActivation?.isActive) return Promise.reject(new Error("Use the Add to draft button."));
    if (typeof text !== "string" || !text.trim() || new TextEncoder().encode(text).length > 8192)
      return Promise.reject(new Error("Enter 1 to 8192 bytes of text."));
    if (pending) return Promise.reject(new Error("A draft request is already in progress."));
    const id = crypto.randomUUID();
    return new Promise((resolve, reject) => {
      pending = {id, resolve, reject};
      try { post(JSON.stringify({nonce, id, text})); }
      catch (e) { pending = undefined; reject(e); }
    });
  } });
  Object.defineProperty(window, "__muqunDraftReply", { value(id,error) {
    if (!pending || pending.id !== id) return;
    const request = pending;
    pending = undefined;
    if (error) request.reject(new Error(error)); else request.resolve();
  } });
  // Context injection can precede this script.
  Object.defineProperty(window, "muqunDesktop", {
    value: Object.freeze({...window.muqunDesktop, appendToDraft: window.__muqunAppendToDraft}), configurable: true
  });
}
