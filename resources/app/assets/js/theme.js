// Dual theme toggle (light / dark)
document.addEventListener('DOMContentLoaded', () => {
  const htmlEl = document.documentElement;
  const themeContainer = document.getElementById('theme-toggle-container');
  const thumb = document.getElementById('theme-thumb');

  if (!themeContainer || !thumb) return;

  function applyTheme(mode) {
    if (mode === 'dark') {
      htmlEl.classList.add('dark');
      localStorage.setItem('theme', 'dark');
      thumb.style.transform = 'translateX(100%)';
    } else {
      // light
      htmlEl.classList.remove('dark');
      localStorage.setItem('theme', 'light');
      thumb.style.transform = 'translateX(0%)';
    }

    const isDark = htmlEl.classList.contains('dark');
    window.dispatchEvent(new CustomEvent('themeChanged', { detail: { isDark } }));
  }

  // Initialize state
  // Check for saved theme preference, otherwise default to dark
  const savedTheme = localStorage.getItem('theme');
  if (savedTheme === 'light') {
    applyTheme('light');
  } else {
    // No saved preference or explicitly dark - default to dark
    applyTheme('dark');
  }

  // Toggle theme when clicking anywhere on the container
  themeContainer.addEventListener('click', () => {
    const isDark = htmlEl.classList.contains('dark');
    applyTheme(isDark ? 'light' : 'dark');
  });
});