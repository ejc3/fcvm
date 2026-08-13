if (typeof RTPRequireScript !== 'function') {

	var RTPRequireScriptBucketLevel = { dfp: '1', analytics: '1', pipe: '1', chartbeat: '1', peach: '1', gemius: '1', sharethis: '1', disqus: '1' }

	var RTPRequireScript = function ( params ) {
		var hasBucket = function () { return true; }
		var loadTrusteScript = function () { return true; }

		return { hasBucket: hasBucket, loadTrusteScript: loadTrusteScript }
	}
}


// analytics
if ( RTPRequireScript( RTPRequireScriptBucketLevel.analytics ).hasBucket() ) {
	(function(i,s,o,g,r,a,m){i['GoogleAnalyticsObject']=r;i[r]=i[r]||function(){ (i[r].q=i[r].q||[]).push(arguments)},i[r].l=1*new Date();a=s.createElement(o), m=s.getElementsByTagName(o)[0];a.async=1;a.src=g;m.parentNode.insertBefore(a,m) })(window,document,'script','//www.google-analytics.com/analytics.js','ga'); ga('create', 'UA-312913-1', 'rtp.pt'); ga('require', 'displayfeatures'); ga('send', 'pageview');ga('create', 'UA-312913-25', 'auto', 'playerTracker');

  // window.addEventListener('locationchange', function () {
  //   console.log('locationchange changed!');
  // });
  // window.addEventListener('hashchange', function() { 
  //   console.log('hashchange changed!');
  // });
  // window.addEventListener('popstate', function() { 
  //   console.log('popstate changed!');
  // });
  // console.log("document.referrer", encodeURIComponent(document.referrer));
  
  if (typeof window.navigation === 'object'){
    window.navigation.addEventListener("navigate", (event) => {
      _sensorAnalytics();
    });
  }else{
    window.addEventListener('hashchange', function () {
      _sensorAnalytics();
    });
  }


  var _sensorAnalytics = function() {
    const _sensorBaseUrl = 'https://www.rtp.pt/rtp-sensor/';
    const _encodeReferrer = document.referrer;
    const _encodeURL = document.URL;

    const _path = window.location.host + window.location.pathname;

    const _sensorRegistration = function(params) {
      const callback_func = params.sensor.callback;
      delete params.sensor.callback;
      const queryParams = new URLSearchParams(params.sensor).toString();
      const urlComQuery = `${params.baseUrl}?${queryParams}`;

      const client = new XMLHttpRequest();
console.log ("typeof params.callback" );
console.log (typeof params.callback );

      client.open("HEAD", urlComQuery, true);
      client.onreadystatechange = () => {
        // In local files, status is 0 upon success in Mozilla Firefox
        if (client.readyState === XMLHttpRequest.DONE) {
          const status = client.status;
          if (status === 0 || (status >= 200 && status < 400)) {
            // The request has been completed successfully
            console.log("typeof params.callback",typeof callback_func);
            console.log ("VAI CHAMAR O CALBACK");

            if (typeof callback_func !== undefined && typeof callback_func === 'function') {
              console.log ("CAHMEIR CArLLBACK");
              callback_func()
            } 
          } else {
            // Oh no! There has been an error with the request!
          }
        }
      };
      client.send();


      //const client = new XMLHttpRequest();
      //client.open("HEAD", urlComQuery, true);
      //client.send();
    };

    var _sensorClick = function(params, callback=false) {
      console.log ("_sensorClick params.sensor",params);
     // const callback_func = params.sensor.callback;
     // delete params.sensor.callback;
      _sensorRegistration({ baseUrl: _sensorBaseUrl, sensor: params, callback: callback});
    }

    //const _allowedDomains = ['conta.rtp.pt', 'www.rtp.pt', 'media.rtp.pt'];

    const queryString = window.location.search;
    const urlParams = new URLSearchParams(queryString);
     
//
//{
//  app_url: 'conta.rtp.pt/realms/rtp/login-actions/registration',
//  sensor_opts: [{ app_qstring: 'execution', sensor :{type: 'sso', action: 'registrationAttempt', referrer: _encodeReferrer, url: _encodeURL  }}
//  ,                                      {  sensor :{ type: 'sso', action: 'registration', referrer: _encodeReferrer, url: _encodeURL  }}
//]
//},
//{
//  app_url: 'conta.rtp.pt/realms/rtp/login-actions/reset-credentials',
//  sensor_opts: [{sensor: { type: 'sso', action: 'reset-credentials', referrer: _encodeReferrer, url: _encodeURL  }}]
//}, 

//{
//  app_url: 'conta.rtp.pt/realms/rtp/login-actions/authenticate',
//  sensor_opts: [{ app_qstring: 'execution', sensor :{type: 'sso', action: 'loginAttempt', referrer: _encodeReferrer, url: _encodeURL  }}
//    ,                                      {  sensor :{ type: 'sso', action: 'login', referrer: _encodeReferrer, url: _encodeURL  }}
//  ]
//}


    const _rules = [ 
      {
        app_url: 'conta.rtp.pt/realms/rtp/protocol/openid-connect/registrations',
        sensor_opts: [{dom_string: ['Criar a sua conta RTP'], sensor: { type: 'sso', action: 'web_btn_abriu_registo', referrer: _encodeReferrer, url: _encodeURL  }}]

      },
      {
        app_url: 'conta.rtp.pt/realms/rtp/login-actions/registration',
        sensor_opts: [{dom_string: ['Criar a sua conta RTP'], sensor: { type: 'sso', action: 'web_btn_abriu_registo', referrer: _encodeReferrer, url: _encodeURL  }}]

      },
      {
        app_url: 'conta.rtp.pt/realms/rtp/protocol/openid-connect/auth',
        sensor_opts: [{ sensor: { type: 'sso', action: 'web_btn_abriu_login', referrer: _encodeReferrer, url: _encodeURL  }}
        ,{ referrer_string: ['webview.html'],  sensor: { type: 'sso', action: 'app_btn_abriu_login', referrer: _encodeReferrer, url: _encodeURL  }}
      ]
      },
      {
        app_url: 'conta.rtp.pt/realms/rtp/login-actions/authenticate',
        sensor_opts: [{dom_string: ['Inicie a sua sess'], sensor: { type: 'sso', action: 'web_btn_abriu_login', referrer: _encodeReferrer, url: _encodeURL  }
      }
      ]
      },
      {
        // Paginad de confirmação de email e email verificado VERIFICADO OK
        app_url: 'conta.rtp.pt/realms/rtp/login-actions/action-token',
        sensor_opts: [ { dom_string: ['Confirme a validade do endere'], app_qstring: 'key',  sensor :{ type: 'sso', action: 'pagina_de_confirmacao_de_email', referrer: _encodeReferrer, url: _encodeURL  }},
                      { dom_string: ['e-mail foi confirmado.'] , app_qstring: 'key',  sensor :{ type: 'sso', action: 'email_verificado', referrer: _encodeReferrer, url: _encodeURL  }}
      ]
      },
      {
        // Envio de email para confirmação  VERIFICADO OK
        app_url: 'conta.rtp.pt/realms/rtp/login-actions/required-action',
        sensor_opts: [ {  dom_string: ['Para confirmar o seu registo, aceda','Tem de verificar o seu endereço de e-mail para ativar sua conta'], sensor :{ type: 'sso', action: 'email_envio_confirmacao', referrer: _encodeReferrer, url: _encodeURL  }}
        ]
      }
      
    ];

    // Function to check if all strings in dom_string exist in the DOM
    var checkAllDomStringsExist = function(strings) {
      console.log (strings);

          // Check if all strings in dom_string exist in the document's body text
          return strings.every(str => document.body.textContent.includes(str));
     
    }
    // Function to check if all strings in dom_string exist in the DOM
    var checkAllReferrerExist = function(strings) {
      console.log (strings);

          // Check if all strings in dom_string exist in the document's body text
          return strings.every(str => _encodeReferrer.includes(str));

    }
    var _sensorFindString = function(param) {
      return document.body.textContent.includes(param); 
    }

    // Function to check if all strings in dom_string exist in the DOM
    var checkAllDomClassExist = function(classes) {
      console.log (classes);
      // Check if all strings in dom_string exist in the document's body text
      return classes.every(str => !!document.querySelector(`.${str}`));
     
    }
    var _sensorFindClass = function(param) {
      return !!document.querySelector(`.${param}`);
    }

    // Function to check if all strings in dom_string exist in the DOM
    var checkAllDomIDExist = function(ids) {
      console.log (ids);
      // Check if all strings in dom_string exist in the document's body text
      return ids.every(str => !!document.getElementById(str));
      
    }
    var _sensorFindId = function(param) {
      return !!document.getElementById(param);
    }



    document.addEventListener('DOMContentLoaded', function() {
 

        _rules.forEach(rule => {
          //console.log(`Processing URL: ${rule.app_url}`);
      
          let paramValue = null;
          if ( urlParams.size > 0 && typeof rule.app_qstring !== 'undefined'){
            paramValue = urlParams.get(rule.app_qstring);
            //paramValue = urlParams.includes(rule.app_qstring);
          }
          let sensorKey = 0;
          if (_path === rule.app_url ) {

          rule.sensor_opts.forEach((option, index) => {
            //console.log(`\nProcessing Sensor Option ${index + 1}:`);
            
            // Extract and display sensor information
            if (option.sensor) {
              const { type, action, referrer, url } = option.sensor;
              //console.log(`  Sensor Type: ${type}`);
              //console.log(`  Action: ${action}`);
              //console.log(`  Referrer: ${referrer}`);
              //console.log(`  URL: ${url}`);
            }
      
            // Extract and display dom_string if available
            let string_needed = 0;
            if (option.dom_string) {
              string_needed = 1;
              //console.log('  DOM Strings:');
              allStrings = checkAllDomStringsExist(option.dom_string);
              //console.log(allStrings ? '------------------ All strings exist' : '------------------ Some strings are missing');
              if (allStrings) {
                string_needed = 2;
              }

              /*
              option.dom_string.forEach((str, i) => {
                console.log(`    [${i + 1}] ${str}`);
                if (_sensorFindString(str)) {
                  string_needed = 2;
                  console.log(`String "${str}" encontrada na página.`);
                }else{
                  console.log(`String "${str}" NAO encontrada na página.`);
                }

              });
              */
            }

            // Extract and display referrer_needed if available
            let referrer_needed = 0;
            if (option.referrer_string) {
              referrer_needed = 1;
              //console.log('  DOM referrer_string:');
              allReferrerStrings = checkAllReferrerExist(option.referrer_string);
              //console.log(allReferrerStrings ? '------------------ All REFERER_ strings exist' : '------------------ Some REFERRER_strings are missing');
              if (allReferrerStrings) {
                referrer_needed = 2;
              }
            }
            // Extract and display dom_id if available
            let id_needed = 0;
            if (option.dom_id) {
              id_needed = 1;
              //console.log('  DOM IDs:');
              allIDs = checkAllDomIDExist(option.dom_id);
              //console.log(allIDs ? '------------------ All IDS exist' : '------------------ Some IDS are missing');
              if (allIDs) {
                id_needed = 2;
              }

              /*
              option.dom_id.forEach((id, i) => {
                console.log(`    [${i + 1}] ${id}`);
                if (_sensorFindId(id)) {
                  id_needed = 2;
                  console.log(`ID "${id}" encontrado na página.`);
                }else{
                  console.log(`ID "${id}" NAO encontrado na página.`);
                }

              });
              */
            }
      
            // Extract and display dom_class if available
            let class_needed = 0;
            if (option.dom_class) {
              class_needed = 1;
              //console.log('  DOM Classes:');
              allClasses = checkAllDomClassExist(option.dom_class);
              //console.log(allClasses ? '------------------ All CLASSES exist' : '------------------ Some CLASSES are missing');
              if (allClasses) {
                class_needed = 2;
              }
/*
              option.dom_class.forEach((cls, i) => {
                console.log(`    [${i + 1}] ${cls}`);
                if (_sensorFindClass(cls)) {
                  console.log(`Classe "${cls}" encontrada na página.`);
                  class_needed = 2;
                } else{
                  console.log(`Classe "${cls}" NAO encontrada na página.`);
                }
              });
              */
            }

            if ((referrer_needed===0 || referrer_needed===2) && (string_needed===0 || string_needed===2) && (id_needed===0 || id_needed===2) && (class_needed===0 || class_needed===2)){
              //console.log(`\nRULE PARA APLICAR ${index}:`);
              //console.log(`\nRULE `,option);
              sensorKey = index;

            }
      
            //console.log('---');
          });
        

        //console.log ("VAI ENVIAR ->>>>",rule.app_url, sensorKey);
        //console.log ("VAI ENVIAR ->>>>",rule.sensor_opts[sensorKey]);

        _sensorRegistration({ baseUrl: _sensorBaseUrl, sensor: rule.sensor_opts[sensorKey].sensor});


      } 

    });
  });
    //}
    return {
      _sensorClick: _sensorClick
    }
  };

  _sensorAnalytics();
 
}




// analytics


// pipe
//if (typeof hidepipe === 'undefined') {
	if ( RTPRequireScript( RTPRequireScriptBucketLevel.pipe ).hasBucket() ) {
		!function(){window.EBUPipeQName="_pipe","_pipe"in window||(window._pipe=function(){
		window._pipe.q.push(arguments)},window._pipe.q=[]),window._pipe.startTime=(new Date).getTime();
		var e=document.createElement("script");e.src="//d3kyk5bao1crtw.cloudfront.net/pipe-latest.min.js",
		e.async=!0;var i=document.getElementsByTagName("script")[0];i.parentNode.insertBefore(e,i)}();

		_pipe('register', 'ptrtp00000000014');
		_pipe('collect', 'pageview');
	}
//}
// pipe

// chartbeat
if ( RTPRequireScript( RTPRequireScriptBucketLevel.chartbeat ).hasBucket() ) {

  if (typeof CHARTBEAT_rtp_domain === 'undefined' || (CHARTBEAT_rtp_domain !== 'play.rtp.pt' && CHARTBEAT_rtp_domain !== 'noticias.rtp.pt')) {
    CHARTBEAT_rtp_domain = false;
  }

  if (CHARTBEAT_rtp_domain) {

    if (typeof CHARTBEAT_rtp_sections === 'undefined') {
      CHARTBEAT_rtp_sections = 'RTP';
    }

    var _sf_async_config = _sf_async_config || {};
    //CONFIGURATION START
    _sf_async_config.uid = 60571;
    _sf_async_config.domain = CHARTBEAT_rtp_domain;
    _sf_async_config.useCanonical = true;
    _sf_async_config.sections = CHARTBEAT_rtp_sections;
    _sf_async_config.authors = 'RTP';
    //CONFIGURATION END
    (function () {
        function loadChartbeat() {
            window._sf_endpt = (new Date()).getTime();
            var e = document.createElement('script');
            e.setAttribute('language', 'javascript');
            e.setAttribute('type', 'text/javascript');
            e.setAttribute('src', '//static.chartbeat.com/js/chartbeat.js');
            document.body.appendChild(e);
        }
        loadChartbeat();
    })();
  }
}
// chartbeat



/*
(function ()  {
  document.addEventListener('DOMContentLoaded', function(event) {

    var getCookie = function( cookieName ) {
      var v = new RegExp(cookieName+"=([^;]+)").exec(document.cookie);
      return (v != null) ? unescape(v[1]) : null;
    }

    var getParentUrl = function() {
      var isInIframe = (parent !== window),
          parentUrl = null;

      if (isInIframe) {
        parentUrl = document.referrer;
      }
      return parentUrl;
    }

    var getHostName = function(url) {
      if (url == null) {
        return null;
      }

      var match = url.match(/:\/\/(www[0-9]?\.)?(.[^/:]+)/i);
      if (match != null && match.length > 2 && typeof match[2] === 'string' && match[2].length > 0) {
        return match[2];
      }
      else {
        return null;
      }
    }

    var getDomain = function(url) {
      var hostName = getHostName(url);
      var domain = hostName;

      if (hostName != null) {
        var parts = hostName.split('.').reverse();
        if (parts != null && parts.length > 1) {
          domain = parts[1] + '.' + parts[0];

          if (hostName.toLowerCase().indexOf('.co.uk') != -1 && parts.length > 2) {
            domain = parts[2] + '.' + domain;
          }
        }
      }
      return {'domain':domain};
    }


    var showCookie = true;
    var parentUrl = getParentUrl();
    var cookieName = '__rtpgeralcookie';
    var cookie = getCookie( cookieName );
    var mensage = 'Este site utiliza cookies para permitir uma melhor experi&ecirc;ncia por parte do utilizador. Ao navegar no site estar&aacute; a consentir a sua utiliza&ccedil;&atilde;o. ';


    var tplContent = function( mensage, callback ) {
      var html = '';
        html += '<div id="rtpgeralcookiecontent" class="rtpgeralcookiecontent">';
          html += '<p class="text-center">' + mensage;
            html += '<a href="http://media.rtp.pt/empresa/utilizacao/cookies/"><u>Saber mais</u></a>';
          html += '</p>';
          html += '<span><button id="rtpgeralcookieagree" title="Concordar e esconder a barra durante 90 dias">OK</button></span>';
        html += '</div>';

      $("body").append(html);
      $(".rtpgeralcookiecontent").css({ "border-top": "1px solid silver", "position": "fixed", "display": "table", "left": "0", "bottom": "0", "width": "100%", "z-index": "1000", "background-color": "white", "padding": "9px" });
      $(".rtpgeralcookiecontent p, .rtpgeralcookiecontent button").css({ "display": "table-cell", "vertical-align": "middle" });
      $(".rtpgeralcookiecontent button").css({ "float": "right" });

      callback();
    }


    if ( parentUrl !== null ) {
      var urlParts = getDomain( parentUrl );
      if  (urlParts['domain'] !== "rtp.pt") {
        showCookie = false;
      }
    }


    if (cookie == null && showCookie) {
      tplContent( mensage, function(){
        var bar = document.getElementById( 'rtpgeralcookiecontent' );
        var btn = document.getElementById( 'rtpgeralcookieagree' );

        btn.addEventListener("click", function(){
          bar.style.display = 'none';
          var d = new Date;
          d.setTime(d.getTime()+24*60*60*1000*90);
          document.cookie = cookieName + "=ok;path=/;expires="+d.toGMTString();
        });
      });
    }

  });
})();

*/
