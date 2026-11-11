/* Arcade V1.0: arcade.realtime for app pages, the original arcade's realtime.js API call for call.
   Rooms travel the arcade mesh. join() never rejects: a room it can't join comes back offline, with
   why, so apps keep working alone.

     arcade.realtime.join('town', {game, cap, fast, link, max})  -> Room
       room.on('message' | 'join' | 'leave' | 'closed' | 'rejoined' | 'reconnecting', fn)
       room.send(value)  room.members()  room.leave()  room.me  room.online  room.why
     room.fast (a room joined with {fast: true}): on('message' | 'peer'), send(value, {reliable, to}),
       routes(), link(ids, {max}), peers()
     room.media (on a fast room): start, stop, showScreen, hideScreen, share, frames, levels, spatial,
       hear, volume, place, mute; on('media' | 'media-off' | 'state' | 'speaking' | 'frame' | 'levels')
     arcade.realtime.area('world', {cell, view, x, y})  -> Area: on, move(x, y), send, members, leave
     arcade.realtime.ring(address, {line, link})  arcade.realtime.ringed()

   A player is {id, address, tag, guest, node}. A message is a string, or any JSON-able value sent as
   JSON, of at most 512 bytes; about five a second per player. */
(function () {
  'use strict';
  if (window.arcade && window.arcade.realtime) return;
  var seq = 0, waiting = {}, rooms = {};

  // A window that is waiting on the person says {pending: why} first: the wait grows to PENDING_WAIT.
  var PENDING_WAIT = 600000;

  function ask(message, wait) {
    return new Promise(function (resolve, reject) {
      message.arcade = 'realtime';
      message.seq = ++seq;
      var w = waiting[message.seq] = { resolve: resolve, reject: reject, pending: '' };
      w.giveUp = function (after) {
        clearTimeout(w.timer);
        w.timer = setTimeout(function () {
          if (waiting[message.seq] !== w) return;
          delete waiting[message.seq];
          reject(new Error(w.pending === 'unlock'
            ? 'wallet-locked: unlock your wallet in the viewer to play online'
            : 'no viewer is listening: open this page in a DogecoinArcade viewer'));
        }, after);
      };
      w.giveUp(wait || 20000);
      window.parent.postMessage(message, '*');
    });
  }

  window.addEventListener('message', function (e) {
    var m = e.data || {};
    if (m.arcade !== 'realtime') return;
    if (m.event) {                                  // pushed: something happened in a room
      var room = rooms[m.event.room];
      if (room) room._hear(m.event);
      return;
    }
    var w = waiting[m.seq];
    if (!w) return;
    if (m.pending) { w.pending = String(m.pending); w.giveUp(PENDING_WAIT); return; }
    clearTimeout(w.timer);
    delete waiting[m.seq];
    if (m.ok) w.resolve(m); else w.reject(new Error(m.error || 'refused'));
  });

  function Room(name, said) {
    this.name = name;
    this.online = !!said.online;
    this.me = said.me || null;
    this.why = said.why || '';
    this._members = {};
    this._on = { message: [], join: [], leave: [], closed: [], rejoined: [], reconnecting: [] };
    var self = this;
    (said.members || []).forEach(function (p) { self._members[p.id] = p; });
    this.fast = said.fast ? new Fast(this) : null;
    this.media = said.fast && said.media ? new Media(this, !!said.screen) : null;
  }

  // Camera, microphone and screen, on a fast room. The window opens them (a sandboxed page cannot)
  // and draws the pictures over the page where it says.
  function Media(room, screen) {
    this.room = room;
    this.screen = screen;                           // this window can share its screen
    this._on = { media: [], 'media-off': [], state: [], speaking: [], frame: [], levels: [] };
  }
  Media.prototype.on = function (type, fn) {
    if (!this._on[type]) throw new Error('media events are: media, media-off, state, speaking, frame, levels');
    this._on[type].push(fn);
    return this;
  };
  Media.prototype._ask = function (what, extra, wait) {
    var m = { op: 'media', room: this.room.name, do: what };
    for (var k in extra || {}) m[k] = extra[k];
    return ask(m, wait);
  };
  function okOrError(promise) {
    return promise.then(function () { return { ok: true }; }, function (err) { return { ok: false, error: err.message }; });
  }
  Media.prototype.start = function (options) {
    options = options || { video: true, audio: true };
    return this._ask('start', { video: !!options.video, audio: !!options.audio, screen: !!options.screen, sound: !!options.sound,
                                facing: options.facing === 'environment' ? 'environment' : options.facing === 'user' ? 'user' : '' }, 600000)
      .then(function (said) { var r = { ok: true, video: !!said.video, audio: !!said.audio, screen: !!said.screen }; if (said.sound) r.sound = true; return r; },
            function (err) { return { ok: false, error: err.message }; });
  };
  Media.prototype.stop = function () { return this._ask('stop'); };
  Media.prototype.showScreen = function (options) {
    return this._ask('screen', { on: true, sound: !!(options && options.sound) }, 600000)
      .then(function (said) { return said.sound ? { ok: true, sound: true } : { ok: true }; }, function (err) { return { ok: false, error: err.message }; });
  };
  Media.prototype.hideScreen = function () { return this._ask('screen', { on: false }); };
  // on: true / false, or {video, audio} for one kind only.
  Media.prototype.share = function (id, on) {
    var said = on && typeof on === 'object' ? { video: !!on.video, audio: !!on.audio } : on !== false;
    return okOrError(this._ask('share', { peer: String(id), on: said }));
  };
  Media.prototype.frames = function (key, size) {
    var k = key === 'me' || key === 'me#screen' ? key : String(key);
    return okOrError(this._ask('frames', { key: k, size: size ? { w: Number(size.w), h: Number(size.h), fps: Number(size.fps || 15) } : null }, 600000));
  };
  Media.prototype.levels = function (keys) { return okOrError(this._ask('levels', { keys: (keys || []).map(String) }, 600000)); };
  Media.prototype.spatial = function (scene) { return this._ask('spatial', { scene: scene || null }); };
  Media.prototype.hear = function (id, on) { return this._ask('hear', { peer: String(id), on: on !== false }); };
  Media.prototype.loudest = 4;
  Media.prototype.volume = function (id, level) { return this._ask('volume', { peer: String(id), level: Number(level) }); };
  // place(id | 'me', {x, y, w, h} in this page's CSS pixels, or null to hide, {fit, mirror})
  Media.prototype.place = function (who, rect, options) {
    options = options || {};
    return this._ask('place', { who: who === 'me' ? 'me' : String(who),
                                rect: rect ? { x: rect.x, y: rect.y, w: rect.w != null ? rect.w : rect.width,
                                               h: rect.h != null ? rect.h : rect.height } : null,
                                fit: options.fit, mirror: !!options.mirror });
  };
  Media.prototype.mute = function (which) {
    which = which || {};
    var m = {};
    if (typeof which.video === 'boolean') m.video = which.video;
    if (typeof which.audio === 'boolean') m.audio = which.audio;
    if (typeof which.sound === 'boolean') m.sound = which.sound;
    return this._ask('mute', m);
  };
  Media.prototype._emit = function (type, a, b) { Room.prototype._emit.call(this, type, a, b); };

  // The fast lane: sends to the other players many times a second. Plain sends are unordered and
  // never resent; {reliable: true} sends arrive, in order. A pair with no direct link is in state
  // 'room': its sends ride the room at about three a second, reliable ones in order and of the rest
  // only the latest.
  function Fast(room) {
    this.room = room;
    this._peers = {};
    this._on = { message: [], peer: [] };
  }
  Fast.prototype.on = function (type, fn) {
    if (!this._on[type]) throw new Error('fast lane events are: message, peer');
    this._on[type].push(fn);
    return this;
  };
  Fast.prototype.send = function (value, options) {
    options = options || {};
    var data;
    if (value instanceof ArrayBuffer) data = value.slice(0);
    else if (ArrayBuffer.isView(value)) data = value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength);
    else data = typeof value === 'string' ? value : JSON.stringify(value);
    var m = { arcade: 'realtime', seq: 0, op: 'fast', room: this.room.name, data: data, reliable: !!options.reliable };
    if (options.to != null) m.to = [].concat(options.to).map(String);
    window.parent.postMessage(m, '*');              // many a second: never answered
  };
  // How each link goes, by player id: 'direct', 'relay', 'cloudflare', 'connecting' or 'none'.
  Fast.prototype.routes = function () {
    return ask({ op: 'fastroutes', room: this.room.name }).then(function (said) { return said.routes || {}; }, function () { return {}; });
  };
  // With whom this player links: an array of player ids, or null for everybody. max: how many links
  // at most. Answers {ok, fast}.
  Fast.prototype.link = function (ids, options) {
    return ask({ op: 'fastlink', room: this.room.name, link: ids == null ? null : [].concat(ids).map(String),
                 max: (options && options.max) | 0 }).then(function (said) { return { ok: true, fast: !!said.fast }; },
                                                           function (err) { return { ok: false, error: err.message }; });
  };
  // {player id: 'connecting' | 'direct' | 'room'}
  Fast.prototype.peers = function () {
    var out = {};
    for (var k in this._peers) out[k] = this._peers[k];
    return out;
  };
  Fast.prototype._emit = function (type, a, b) { Room.prototype._emit.call(this, type, a, b); };

  Room.prototype.on = function (type, fn) {
    if (!this._on[type]) throw new Error('events are: message, join, leave, closed, rejoined, reconnecting');
    this._on[type].push(fn);
    return this;
  };
  Room.prototype.members = function () {
    var out = [];
    for (var k in this._members) out.push(this._members[k]);
    return out;
  };
  Room.prototype.send = function (value) {
    if (!this.online) return Promise.reject(new Error('this room is not online'));
    return ask({ op: 'send', room: this.name, data: typeof value === 'string' ? value : JSON.stringify(value) });
  };
  Room.prototype.leave = function () {
    delete rooms[this.name];
    this.online = false;
    return ask({ op: 'leave', room: this.name }).catch(function () {});
  };
  Room.prototype._emit = function (type, a, b, c) {
    this._on[type].forEach(function (fn) { try { fn(a, b, c); } catch (err) { setTimeout(function () { throw err; }); } });
  };
  Room.prototype._hear = function (ev) {
    var p = ev.member;
    if (ev.type === 'join') { this._members[p.id] = p; this._emit('join', p); }
    else if (ev.type === 'leave') { delete this._members[p.id]; this._emit('leave', p); }
    else if (ev.type === 'closed') { this.online = false; this._emit('closed', ev.why || ''); }
    else if (ev.type === 'me') { this.me = p; }
    // This page's own seat was taken again: the others saw it leave and come back, so send your state
    // again. The second argument says what of the camera, microphone and screen stayed open, or null.
    else if (ev.type === 'rejoined') { this.me = p || this.me; this.online = true; this._emit('rejoined', this.me, ev.media || null); }
    else if (ev.type === 'reconnecting') { this.online = false; this._emit('reconnecting'); }
    else if (ev.type === 'fast' && this.fast) {
      var said = ev.data;
      if (typeof said === 'string') { try { said = JSON.parse(said); } catch (err) {} }
      this.fast._emit('message', said, p);
    }
    else if (ev.type === 'media' && this.media) this.media._emit('media', p, { video: !!ev.video, audio: !!ev.audio });
    else if (ev.type === 'media-off' && this.media) this.media._emit('media-off', p);
    else if (ev.type === 'media-speaking' && this.media) this.media._emit('speaking', p, !!ev.on);
    else if (ev.type === 'media-frame' && this.media) {
      var fr = this.media._on.frame;
      if (!fr.length) { try { ev.bitmap.close(); } catch (err) {} }
      else fr.forEach(function (fn) { try { fn(ev.key, ev.bitmap, ev.t, p); } catch (err) { setTimeout(function () { throw err; }); } });
      this.media._ask('shown', { key: ev.key, t: ev.t }).catch(function () {});
    }
    else if (ev.type === 'media-levels' && this.media) this.media._emit('levels', ev.levels || {});
    else if (ev.type === 'media-state' && this.media) this.media._emit('state', ev.state);
    else if (ev.type === 'peer' && this.fast) {
      if (ev.state === 'gone') delete this.fast._peers[p.id]; else this.fast._peers[p.id] = ev.state;
      this.fast._emit('peer', p, ev.state);
    }
    else if (ev.type === 'message') {
      var data = ev.data;
      if (typeof data === 'string') { try { data = JSON.parse(data); } catch (err) {} }
      else if (ev.b64) data = ev.b64;
      this._emit('message', data, p);
    }
  };

  // ring(address, {line, link}) -> {ok, rung, why}; ringed() -> {from, tag, link, line, at} when a
  // ring opened this app, else null.
  function ring(address, options) {
    options = options || {};
    return ask({ op: 'ring', room: '', to: String(address || ''), line: String(options.line || '').slice(0, 80),
                 link: String(options.link || '').slice(0, 200) }, 60000)
      .then(function (said) { return { ok: true, rung: !!said.rung, why: said.why || '' }; },
            function (err) { return { ok: false, error: err.message }; });
  }
  function ringed() {
    return ask({ op: 'ringed', room: '' }).then(function (said) { return said.ring || null; }, function () { return null; });
  }

  // join(room, {game}): `game` defaults to this inscription, so only copies of the same page meet.
  function joinOne(name, options) {
    if (rooms[name]) return Promise.resolve(rooms[name]);
    var m = { op: 'join', room: name, game: options.game || '', fast: !!options.fast, waits: true };
    if (Array.isArray(options.link)) m.link = options.link.map(String);
    if (options.max) m.max = options.max | 0;
    return ask(m).then(function (said) {
      var room = new Room(name, said);
      if (room.online) rooms[name] = room;
      return room;
    }, function (err) {
      return new Room(name, { online: false, why: String(err && err.message || err) });
    });
  }

  function join(name, options) {
    options = options || {};
    name = String(name || '');
    var cap = options.cap | 0;
    if (!cap) return joinOne(name, options);
    // A full room sends you on to the next of its name: town, town#2, ...
    function tryN(n) {
      var label = n === 1 ? name : name + '#' + n;
      return joinOne(label, options).then(function (room) {
        if (!room.online || n >= 50) return room;
        var others = room.members().filter(function (p) { return !room.me || p.id !== room.me.id; });
        if (others.length < cap) return room;
        return room.leave().then(function () { return tryN(n + 1); });
      });
    }
    return tryN(1);
  }

  // area(base, {game, cell, view, x, y}): the world cut into square cells `cell` units wide. You are
  // in the cells within `view` of where you stand (rooms 'base@cx,cy') and hear those; you say
  // things only in your own. move(x, y) as you walk: cells come and go by themselves.
  function Area(base, options) {
    this.base = base; this.options = options;
    this.cell = Math.max(1, +options.cell || 64);
    this.view = Math.max(0, options.view == null ? this.cell / 2 : +options.view);
    this.rooms = {};            // cell key -> Room
    this.where = {};            // player id -> {cell key: true}
    this.players = {};          // player id -> player
    this.online = false; this.me = null; this.here = null;
    this._on = { message: [], join: [], leave: [], closed: [], rejoined: [], reconnecting: [] };
  }
  Area.prototype.on = Room.prototype.on;
  Area.prototype._emit = Room.prototype._emit;
  Area.prototype.key = function (cx, cy) { return this.base + '@' + cx + ',' + cy; };
  Area.prototype.cellOf = function (x, y) { return [Math.floor(x / this.cell), Math.floor(y / this.cell)]; };
  Area.prototype.wanted = function (x, y) {
    var out = {}, v = this.view, c = this.cell;
    for (var cx = Math.floor((x - v) / c); cx <= Math.floor((x + v) / c); cx++)
      for (var cy = Math.floor((y - v) / c); cy <= Math.floor((y + v) / c); cy++) out[this.key(cx, cy)] = true;
    return out;
  };
  Area.prototype._seen = function (key, p, present) {
    if (this.me && p.id === this.me.id) return;
    var w = this.where[p.id] || (this.where[p.id] = {});
    var before = Object.keys(w).length;
    if (present) w[key] = true; else delete w[key];
    var after = Object.keys(w).length;
    if (!before && after) { this.players[p.id] = p; this._emit('join', p); }
    else if (before && !after) { delete this.where[p.id]; delete this.players[p.id]; this._emit('leave', p); }
  };
  Area.prototype._attach = function (key, room) {
    var self = this;
    this.rooms[key] = room;
    if (room.me) this.me = room.me;
    room.members().forEach(function (p) { self._seen(key, p, true); });
    room.on('join', function (p) { if (self.rooms[key] === room) self._seen(key, p, true); });
    room.on('leave', function (p) { if (self.rooms[key] === room) self._seen(key, p, false); });
    room.on('message', function (data, p) { if (self.rooms[key] === room) self._emit('message', data, p, key); });
    room.on('closed', function (why) { if (self.rooms[key] === room) { delete self.rooms[key]; self._emit('closed', why); } });
    room.on('reconnecting', function () { if (self.rooms[key] === room) self._emit('reconnecting', key); });
    room.on('rejoined', function (me) { if (self.rooms[key] === room) self._emit('rejoined', me, key); });
  };
  Area.prototype.move = function (x, y) {
    var self = this, want = this.wanted(x, y), jobs = [];
    var c = this.cellOf(x, y); this.here = this.key(c[0], c[1]); want[this.here] = true;
    this._want = want;                              // where we are now: a join that ends late checks this
    Object.keys(this.rooms).forEach(function (key) {
      if (want[key]) return;
      var room = self.rooms[key]; delete self.rooms[key];
      if (room === 'joining') return;               // its join will see it is no longer wanted
      Object.keys(self.where).forEach(function (id) { if (self.where[id][key]) self._seen(key, self.players[id], false); });
      jobs.push(room.leave());
    });
    Object.keys(want).forEach(function (key) {
      if (self.rooms[key]) return;
      self.rooms[key] = 'joining';
      jobs.push(joinOne(key, self.options).then(function (room) {
        if (!room.online) { if (self.rooms[key] === 'joining') delete self.rooms[key]; return; }
        self.online = true;
        if (self._want[key] && self.rooms[key] === 'joining') self._attach(key, room);
        else { if (self.rooms[key] === 'joining') delete self.rooms[key]; return room.leave(); }
      }));
    });
    return Promise.all(jobs).then(function () { return self; });
  };
  Area.prototype.send = function (value) {
    var room = this.rooms[this.here];
    if (!room || room === 'joining') return Promise.reject(new Error('not in a cell yet'));
    return room.send(value);
  };
  Area.prototype.members = function () {
    var out = [];
    for (var k in this.players) out.push(this.players[k]);
    return out;
  };
  Area.prototype.leave = function () {
    var self = this;
    return Promise.all(Object.keys(this.rooms).map(function (key) {
      var room = self.rooms[key]; delete self.rooms[key];
      return room && room.leave ? room.leave() : null;
    }));
  };
  function area(base, options) {
    options = options || {};
    var a = new Area(String(base || ''), options);
    return a.move(+options.x || 0, +options.y || 0);
  }

  window.arcade = window.arcade || {};
  window.arcade.realtime = { join: join, area: area, ring: ring, ringed: ringed, MAX_BYTES: 512 };
})();
