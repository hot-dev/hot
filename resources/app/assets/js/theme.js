// Dual theme toggle (light / dark)
document.addEventListener('DOMContentLoaded', () => {
  const htmlEl = document.documentElement;
  const themeContainer = document.getElementById('theme-toggle-container');

  if (!themeContainer) return;

  function syncButtonState(isDark) {
    themeContainer.setAttribute('aria-checked', isDark ? 'true' : 'false');
    themeContainer.setAttribute('title', isDark ? 'Switch to light mode' : 'Switch to dark mode');
  }

  function applyTheme(mode, dispatch = true) {
    const isDark = mode === 'dark';
    htmlEl.classList.toggle('dark', isDark);
    localStorage.setItem('theme', isDark ? 'dark' : 'light');
    syncButtonState(isDark);

    if (dispatch) {
      window.dispatchEvent(new CustomEvent('themeChanged', { detail: { isDark } }));
    }
  }

  // Initialize state without redundant animation jump (CSS positions #theme-thumb via html.dark)
  const savedTheme = localStorage.getItem('theme');
  applyTheme(savedTheme === 'light' ? 'light' : 'dark', false);

  // Toggle theme when clicking anywhere on the button
  themeContainer.addEventListener('click', () => {
    const isDark = htmlEl.classList.contains('dark');
    applyTheme(isDark ? 'light' : 'dark', true);
  });
});