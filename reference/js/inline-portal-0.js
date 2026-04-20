function submitAction() {
    var link = document.location.href;
    var searchString = "redirect=";
    var equalIndex = link.indexOf(searchString);
    var redirectUrl = "";
    if (equalIndex >= 0) {
        equalIndex += searchString.length;
        redirectUrl = "http://www.wmich.edu";
        redirectUrl += link.substring(equalIndex);
    }
    if (redirectUrl.length > 255)
        redirectUrl = redirectUrl.substring(0, 255);
    document.forms[0].redirect_url.value = redirectUrl;

    document.forms[0].buttonClicked.value = 4;
    document.forms[0].submit();
}

function loadAction() {
    var url = window.location.href;
    var args = new Object();
    var query = location.search.substring(1);
    var pairs = query.split("&");
    for (var i = 0; i < pairs.length; i++) {
        var pos = pairs[i].indexOf('=');
        if (pos == -1) continue;
        var argname = pairs[i].substring(0, pos);
        var value = pairs[i].substring(pos + 1);
        args[argname] = unescape(value);
    }
    //alert( "AP MAC Address is " + args.ap_mac); 
    //alert( "The controller URL is " + args.switch_url); 
    document.forms[0].action = args.switch_url;
}