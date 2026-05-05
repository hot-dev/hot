function addCopyButtons() {
  document.querySelectorAll('.docs-content pre').forEach(function(pre) {
    if (pre.querySelector('.copy-button')) return;

    var button = document.createElement('button');
    button.className = 'copy-button';
    button.innerHTML = '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>';
    button.title = 'Copy code';

    button.addEventListener('click', function() {
      var code = pre.querySelector('code');
      var text = code ? code.textContent : pre.textContent;

      navigator.clipboard.writeText(text).then(function() {
        button.innerHTML = '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"></polyline></svg>';
        button.classList.add('copied');

        setTimeout(function() {
          button.innerHTML = '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>';
          button.classList.remove('copied');
        }, 2000);
      });
    });

    pre.appendChild(button);
  });
}

function initScrollSpy() {
  var tocLinks = document.querySelectorAll('.toc-link');
  if (tocLinks.length === 0) return;

  var headings = [];
  tocLinks.forEach(function(link) {
    var id = link.getAttribute('href').substring(1);
    var target = document.getElementById(id);
    if (target) headings.push({ id: id, element: target, link: link });
  });
  if (headings.length === 0) return;

  function updateActiveLink() {
    var scrollPos = window.scrollY + 96;
    var active = headings[0];
    for (var i = headings.length - 1; i >= 0; i--) {
      var top = headings[i].element.getBoundingClientRect().top + window.scrollY;
      if (top <= scrollPos) {
        active = headings[i];
        break;
      }
    }
    tocLinks.forEach(function(link) { link.classList.remove('toc-active'); });
    if (active) active.link.classList.add('toc-active');
  }

  window.addEventListener('scroll', updateActiveLink, { passive: true });
  updateActiveLink();
}

window.addEventListener('load', function() {
  if (typeof Prism !== 'undefined') {
    Prism.highlightAll();
  }
  setTimeout(addCopyButtons, 100);
  initScrollSpy();
});
