
const currentUrl = window.location.href;
const domain = window.location.hostname;
 
if (!domain.endsWith('rtp.pt')) {
  const encodedUrl = encodeURIComponent(currentUrl);
 
  fetch(`https://seu-endpoint.com/registo?url=${encodedUrl}`)
    .then(response => response.text())
    .then(data => {
      console.log('Registo enviado com sucesso:', data);
    })
    .catch(error => {
      console.error('Erro ao enviar registo:', error);
    });
}


window.gdprAppliesGlobally=true;(function(){function n(e){if(!window.frames[e]){if(document.body&&document.body.firstChild){var t=document.body;var r=document.createElement("iframe");r.style.display="none";r.name=e;r.title=e;t.insertBefore(r,t.firstChild)}else{setTimeout(function(){n(e)},5)}}}function e(r,a,o,s,c){function e(e,t,r,n){if(typeof r!=="function"){return}if(!window[a]){window[a]=[]}var i=false;if(c){i=c(e,n,r)}if(!i){window[a].push({command:e,version:t,callback:r,parameter:n})}}e.stub=true;e.stubVersion=2;function t(n){if(!window[r]||window[r].stub!==true){return}if(!n.data){return}var i=typeof n.data==="string";var e;try{e=i?JSON.parse(n.data):n.data}catch(t){return}if(e[o]){var a=e[o];window[r](a.command,a.version,function(e,t){var r={};r[s]={returnValue:e,success:t,callId:a.callId};n.source.postMessage(i?JSON.stringify(r):r,"*")},a.parameter)}}if(typeof window[r]!=="function"){window[r]=e;if(window.addEventListener){window.addEventListener("message",t,false)}else{window.attachEvent("onmessage",t)}}}e("__uspapi","__uspapiBuffer","__uspapiCall","__uspapiReturn");n("__uspapiLocator");e("__tcfapi","__tcfapiBuffer","__tcfapiCall","__tcfapiReturn");n("__tcfapiLocator");(function(e){var t=document.createElement("link");t.rel="preconnect";t.as="script";var r=document.createElement("link");r.rel="dns-prefetch";r.as="script";var n=document.createElement("link");n.rel="preload";n.as="script";var i=document.createElement("script");i.id="spcloader";i.type="text/javascript";i["async"]=true;i.charset="utf-8";var a="https://sdk.privacy-center.org/"+e+"/loader.js?target="+document.location.hostname;if(window.didomiConfig&&window.didomiConfig.user){var o=window.didomiConfig.user;var s=o.country;var c=o.region;if(s){a=a+"&country="+s;if(c){a=a+"&region="+c}}}t.href="https://sdk.privacy-center.org/";r.href="https://sdk.privacy-center.org/";n.href=a;i.src=a;var d=document.getElementsByTagName("script")[0];d.parentNode.insertBefore(t,d);d.parentNode.insertBefore(r,d);d.parentNode.insertBefore(n,d);d.parentNode.insertBefore(i,d)})("2840048d-b3c4-47b9-ba5d-c37ed10b41ca")})();


window.didomiOnReady = window.didomiOnReady || [];

window.didomiOnReady.push(function (Didomi) {
});

// window.didomiConfig = {
//   user: {
//     ignoreConsentBefore: "2024-03-11T00:00:00Z",
//     bots: {
//       consentRequired: false,
//       types: ['crawlers', 'performance'],
//       extraUserAgents: [],
//     }
//   }
// };

window.didomiConfig = {
  user: {
    ignoreConsentBefore: "2024-03-20T08:10:00Z",
    bots: {
      consentRequired: false,
      types: ['crawlers', 'performance'],
      extraUserAgents: [],
    }
  },
  integrations: {
    vendors: {
      google: {
        enable: true,
        eprivacy: true
      }
    },
    refreshOnConsent: true
  }
};

window.didomiEventListeners = window.didomiEventListeners || [];

window.didomiEventListeners.push({
  event: 'consent.changed',
  listener: function (context) {

    var rtpOptions = [ null, true, false, false, false ];

    rtpOptions[3] = Didomi.getUserConsentStatusForVendor('google');

    if (Didomi.getUserConsentStatusForPurpose('personaliz-DZEfTQEY')
      && Didomi.getUserConsentStatusForVendor('c:ebu-xJC3fgHb')
      && Didomi.getUserConsentStatusForVendor('c:onesignal-j9tbhF7m')) {
      rtpOptions[2] = true;
    } else {
      rtpOptions[2] = false;
    }

    if (Didomi.getUserConsentStatusForPurpose('cookiesso-nWqGkKGk')
      && Didomi.getUserConsentStatusForVendor('c:sharethis-XWL69FDq')) {
      rtpOptions[4] = true;
    } else {
      rtpOptions[4] = false;
    }

    rtpOptions = [ null, true, rtpOptions[2], rtpOptions[3], rtpOptions[4] ];

    RTPRequireScript().submitUserOptions(rtpOptions);
  }
});

window['RTPRequireScriptBucketLevel'] = {
  dfp: '3', analytics: '1', pipe: '2', peach: '2', chartbeat: '1', gemius: '1', sharethis: '4', disqus: '4', onesignal: '2'
}

window['RTPRequireScript'] = function(params) {
  var rtpOptions = [ null, true, false, false, false ];

  var config = {
    rtpCookieControl: 'rtp_privacy',
    rtpCookieName: 'rtp_cookie_privacy',
    googleCookieName: 'googlepersonalization'
  };

  var RequiredException = function(message) {
    this.message = 'To append [' + message + '] bucket is required!';
    this.name = 'RequiredException';
  }


  var _processCookieRTP = function(params) {
    return params.rtpOptions.map((el, i) => {
      if (el === null) return el
      else if (params.cookieRTP.indexOf(i) > -1) return true
      return false
    })
  }

  var _getCookie = function(cname) {
    var name = cname + "=";
    var decodedCookie = decodeURIComponent(document.cookie);
    var ca = decodedCookie.split(';');

    for(var i = 0; i <ca.length; i++) {
      var c = ca[i];

      while (c.charAt(0) == ' ') {
        c = c.substring(1);
      }

      if (c.indexOf(name) == 0) {
        return c.substring(name.length, c.length);
      }
    }

    return false;
  }

  var isCrowley = function() {
    var botPattern = "(googlebot\/|bot|Googlebot-Mobile|Googlebot-Image|Google favicon|Mediapartners-Google|bingbot|slurp|java|wget|curl|Commons-HttpClient|Python-urllib|libwww|httpunit|nutch|phpcrawl|msnbot|jyxobot|FAST-WebCrawler|FAST Enterprise Crawler|biglotron|teoma|convera|seekbot|gigablast|exabot|ngbot|ia_archiver|GingerCrawler|webmon |httrack|webcrawler|grub.org|UsineNouvelleCrawler|antibot|netresearchserver|speedy|fluffy|bibnum.bnf|findlink|msrbot|panscient|yacybot|AISearchBot|IOI|ips-agent|tagoobot|MJ12bot|dotbot|woriobot|yanga|buzzbot|mlbot|yandexbot|purebot|Linguee Bot|Voyager|CyberPatrol|voilabot|baiduspider|citeseerxbot|spbot|twengabot|postrank|turnitinbot|scribdbot|page2rss|sitebot|linkdex|Adidxbot|blekkobot|ezooms|dotbot|Mail.RU_Bot|discobot|heritrix|findthatfile|europarchive.org|NerdByNature.Bot|sistrix crawler|ahrefsbot|Aboundex|domaincrawler|wbsearchbot|summify|ccbot|edisterbot|seznambot|ec2linkfinder|gslfbot|aihitbot|intelium_bot|facebookexternalhit|yeti|RetrevoPageAnalyzer|lb-spider|sogou|lssbot|careerbot|wotbox|wocbot|ichiro|DuckDuckBot|lssrocketcrawler|drupact|webcompanycrawler|acoonbot|openindexspider|gnam gnam spider|web-archive-net.com.bot|backlinkcrawler|coccoc|integromedb|content crawler spider|toplistbot|seokicks-robot|it2media-domain-crawler|ip-web-crawler.com|siteexplorer.info|elisabot|proximic|changedetection|blexbot|arabot|WeSEE:Search|niki-bot|CrystalSemanticsBot|rogerbot|360Spider|psbot|InterfaxScanBot|Lipperhey SEO Service|CC Metadata Scaper|g00g1e.net|GrapeshotCrawler|urlappendbot|brainobot|fr-crawler|binlar|SimpleCrawler|Livelapbot|Twitterbot|cXensebot|smtbot|bnf.fr_bot|A6-Indexer|ADmantX|Facebot|Twitterbot|OrangeBot|memorybot|AdvBot|MegaIndex|SemanticScholarBot|ltx71|nerdybot|xovibot|BUbiNG|Qwantify|archive.org_bot|Applebot|TweetmemeBot|crawler4j|findxbot|SemrushBot|yoozBot|lipperhey|y!j-asr|Domain Re-Animator Bot|AddThis|Chrome-Lighthouse|Mediapartners-Google)";

    var re = new RegExp(botPattern, 'i');
    var userAgent = navigator.userAgent;

    return re.test(userAgent);
  }

  var _setCookie = function( name, value ) {
    document.cookie = name + "=" + value + "; expires=Fri, 31 Dec 2050 23:59:59 GMT" + ";domain=.rtp.pt;path=/";
  }

  var _setCookieControl = function(name) {
    var value = Date.now();
    var expires = 'Fri, 31 Dec 2050 23:59:59 GMT'

    document.cookie = name + "=" + value + "; expires=" + expires + "; domain=.rtp.pt; path=/";
  }

  var _processedCookieRTP = function() {
    var cookieRTP = _getCookie(config.rtpCookieName);
    return (cookieRTP) ? _processCookieRTP({ cookieRTP, rtpOptions }) : rtpOptions;
  }

  var _loadCookieLevel = function() {
    return (_processedCookieRTP()).map((el, i) => (el) ? i : false).filter(el => el).join(",");
  }

  var _inArray = function(needle, haystack){
    var found = 0;

    for(var i=0, len=haystack.length; i<len; i++) {
      if(haystack[i] == needle) return i;
      found++;
    }

    return -1;
  }

  var _isAllowed = function(settings) {
    if(settings.bucket.length === 0) { return false; }

    var bucketToString =(typeof settings.bucket === 'number') ? settings.bucket.toString() : settings.bucket;
    var bucket = [ bucketToString ];

    for(var x in bucket) {
      if(_inArray(bucket[x], cookieLevel) === -1){
        return false;
      }
    }

    return true;
  }

  var _appendScript = function(settings) {
    if(settings.bucket !== undefined && settings.bucket !== '') {
      if(! _isAllowed(settings)) {
        return false;
      }

      var append =(settings.append === undefined) ? 'body' : settings.append;
      var version =(settings.version === undefined) ? '' : '?vers' + settings.version;

      var s = document.createElement('script');
      s.type = "text/javascript";
      s.src = settings.src + version;
      document.getElementsByTagName(append)[0].appendChild(s);

      if(settings.callback) {
        s.onload = function(){
          settings.callback();
        }
      }
    } else {
      throw new RequiredException(settings.src);
    }
  }

  var _hasBucketString = function(valueInput) {
    return(valueInput !== undefined) ? _isAllowed({ bucket: valueInput }) : false;
  }

  var _hasBucketObject = function(settings) {
    return(settings.bucket !== undefined) ? _isAllowed(settings) : false;
  }



  var self = this;
  var loadedCookieLevel = _loadCookieLevel().split(',');
  var cookieLevel = loadedCookieLevel.map(el=>parseInt(el));

  var submitUserOptions = function (params) {
    var rtpCookieValue = 'permit ' + (params).map((el, i) => (el) ? i : false).filter(el => el).join(",");
    var googleCookieValue = params[3];

    _setCookieControl(config.rtpCookieControl);
    _setCookie(config.rtpCookieName, rtpCookieValue);
    _setCookie(config.googleCookieName, googleCookieValue);
  }

  var getConfig = function() {
    return window['RTPRequireScriptBucketLevel'];
  }

  var toString = function() {
    var p = _processedCookieRTP();
    return 'permit ' + (p).map((el, i) => (el) ? i : false).filter(el => el).join(",");
  }

  var loadScript = function() {
    if (params.length === 0) { return false; }
    for(var x in params) { _appendScript(params[x]); }
  }

  var hasBucket = function() {
    return(typeof(params) === 'object') ? _hasBucketObject(params) : _hasBucketString(params);
  }

  var showConsentLayer = function() {
    Didomi.preferences.show();
  }

  var loadTrusteScript = function () {
    return true;
  }

  var init = function() {
    return self;
  }

  return {
    init: init,
    toString: toString,
    getConfig: getConfig,
    hasBucket: hasBucket,
    loadScript: loadScript,
    cookieLevel: cookieLevel,
    loadTrusteScript: loadTrusteScript,
    showConsentLayer: showConsentLayer,
    submitUserOptions: submitUserOptions
  }
}

RTPRequireScript().init();