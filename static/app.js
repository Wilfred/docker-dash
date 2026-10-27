'use strict';
const $ = id => document.getElementById(id);
let containers = [], overviewBusy = false;
let selected = null, page = 0, until = null, logRequest = 0;
const duration = seconds => {
  if (seconds == null || !Number.isFinite(seconds)) return 'Unavailable';
  seconds = Math.max(0, Math.floor(seconds));
  const days = Math.floor(seconds / 86400), hours = Math.floor(seconds / 3600) % 24, minutes = Math.floor(seconds / 60) % 60;
  return days ? `${days}d ${hours}h` : hours ? `${hours}h ${minutes}m` : minutes ? `${minutes}m` : `${seconds}s`;
};
async function api(url) {
  const response = await fetch(url, {signal: AbortSignal.timeout(20000)});
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || `Request failed (${response.status})`);
  return result;
}
function errorAt(id, message) { $(id).textContent = message || ''; $(id).hidden = !message; }
function cell(row, text, className) {
  const td = document.createElement('td'); td.textContent = text;
  if (className) td.className = className;
  row.append(td); return td;
}
function render() {
  const query = $('search').value.toLowerCase();
  const filtered = containers.filter(c => `${c.name} ${c.image} ${c.version}`.toLowerCase().includes(query));
  $('containers').replaceChildren();
  for (const c of filtered) {
    const row = document.createElement('tr');
    const name = cell(row, '');
    const title = document.createElement('strong'); title.textContent = c.name;
    const status = document.createElement('span'); status.className = 'running'; status.textContent = `● ${c.status}`;
    title.append(status);
    const id = document.createElement('span'); id.className = 'container-id'; id.textContent = c.id.slice(0, 12);
    name.append(title, id);
    cell(row, c.image, 'image');
    const tag = document.createElement('span'); tag.className = 'tag'; tag.textContent = c.version;
    cell(row, '').append(tag);
    const uptime = cell(row, duration(c.uptime_seconds)); uptime.title = `Started ${c.started_at}`;
    const button = document.createElement('button'); button.className = 'log-button'; button.textContent = 'View logs ↗';
    button.setAttribute('aria-label', `View logs for ${c.name}`);
    button.addEventListener('click', () => {
      selected = c; page = 0; until = null;
      $('log-title').textContent = c.name;
      $('log-panel').hidden = false;
      loadLogs(); $('log-panel').scrollIntoView({behavior:'smooth', block:'nearest'});
    });
    cell(row, '').append(button); $('containers').append(row);
  }
  $('empty').hidden = filtered.length > 0;
  $('empty').textContent = containers.length ? 'No containers match your filter.' : 'No running containers. Start a container and refresh to see it here.';
}
async function refresh() {
  if (overviewBusy) return;
  overviewBusy = true; $('refresh').disabled = true;
  try {
    const data = await api('/api/overview');
    containers = data.containers;
    $('count').textContent = containers.length; $('badge').textContent = containers.length;
    $('version').textContent = data.docker_version || 'Unavailable';
    $('uptime').textContent = duration(data.host_uptime_seconds);
    $('updated').textContent = `Updated ${new Date(data.checked_at).toLocaleTimeString()}`;
    errorAt('error', null); render();
  } catch (error) {
    errorAt('error', `Unable to reach Docker. Check that the daemon is running and this user has socket access.\n${error.message}`);
    $('updated').textContent = 'Connection failed · displayed data may be stale';
    if (!containers.length) $('empty').textContent = 'Container information is unavailable.';
  } finally { overviewBusy = false; $('refresh').disabled = false; }
}
async function loadLogs() {
  if (!selected) return;
  const request = ++logRequest;
  const requestedPage = page;
  ['latest','older','newer'].forEach(id => $(id).disabled = true);
  $('logs').textContent = 'Loading…'; errorAt('log-error', null);
  try {
    const query = new URLSearchParams({page: requestedPage});
    if (until) query.set('until', until);
    const data = await api(`/api/containers/${encodeURIComponent(selected.id)}/logs?${query}`);
    if (request !== logRequest) return;
    until = data.until;
    $('logs').textContent = data.lines.length ? data.lines.join('\n') : 'No log output available for this page.';
    $('log-mode').textContent = `${requestedPage === 0 ? 'Latest output' : `History · page ${requestedPage + 1}`} · snapshot ${new Date(until).toLocaleTimeString()}${data.history_limit_reached ? ' · history limit reached' : ''}`;
    $('older').disabled = !data.has_older; $('newer').disabled = requestedPage === 0;
  } catch (error) {
    if (request !== logRequest) return;
    errorAt('log-error', error.message); $('logs').textContent = 'Logs unavailable. The container may have stopped, or its logging driver may not support reading logs.';
    $('newer').disabled = page === 0;
  } finally { if (request === logRequest) $('latest').disabled = false; }
}
$('refresh').addEventListener('click', refresh);
$('search').addEventListener('input', render);
$('latest').addEventListener('click', () => { page = 0; until = null; loadLogs(); });
$('older').addEventListener('click', () => { page++; loadLogs(); });
$('newer').addEventListener('click', () => { page--; loadLogs(); });
$('close-logs').addEventListener('click', () => { selected = null; logRequest++; $('log-panel').hidden = true; });
refresh();
setInterval(() => { if (!document.hidden) refresh(); }, 15000);
