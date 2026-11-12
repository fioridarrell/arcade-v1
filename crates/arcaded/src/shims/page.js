/* Arcade V1.0: this page's pass for the page API, put first in an app's page by the arcade that serves it. The
   page's own requests to /r/ on this arcade (fetch and XMLHttpRequest) carry it as a header, which another
   website can neither read nor add; without it /r/ answers nothing. */
(function () {
  'use strict';
  var pass = '__PASS__', here;
  try { here = new URL(location.href).origin; } catch (e) { return; }
  function ours(u) {
    try { var x = new URL(String(u), location.href); return x.origin === here && x.pathname.indexOf('/r/') === 0; } catch (e) { return false; }
  }
  var f = window.fetch;
  if (f) {
    window.fetch = function (input, init) {
      var url = input && typeof input === 'object' && 'url' in input ? input.url : input;
      if (!ours(url)) return f.apply(window, arguments);
      var req = new Request(input, init);
      req.headers.set('X-Arcade-Page', pass);
      return f.call(window, req);
    };
  }
  var X = window.XMLHttpRequest && window.XMLHttpRequest.prototype;
  if (X) {
    var open = X.open, send = X.send;
    X.open = function (method, url) { this._arcadePage = ours(url); return open.apply(this, arguments); };
    X.send = function () {
      if (this._arcadePage) { try { this.setRequestHeader('X-Arcade-Page', pass); } catch (e) { /* not opened */ } }
      return send.apply(this, arguments);
    };
  }
})();
