const status = document.getElementById('status');
const refresh = document.getElementById('refresh');

async function updateHealth() {
  refresh.disabled = true;
  try {
    const { electron, profile, phase, uiReady } = await window.cordisDev.health();
    document.getElementById('electron').textContent = electron;
    document.getElementById('profile').textContent = profile;
    document.getElementById('phase').textContent = phase;
    status.textContent = uiReady ? 'Cordis 已连接，应用已就绪。' : 'Cordis 已连接，正在等待应用就绪。';
  } catch {
    status.textContent = 'Cordis 连接已断开。';
  } finally {
    refresh.disabled = false;
  }
}

refresh.addEventListener('click', updateHealth);
updateHealth();
