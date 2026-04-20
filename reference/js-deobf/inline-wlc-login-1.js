function getErrorMsgIfAny() {
	if (document.forms[0].err_flag.value == 1) {
		document.writeln("      <tr align=\"center\"> <td colspan=\"2\" style=\"color:#CC0000\">Login Error.</td>     </tr><tr align=\"center\"> <td width=\"350\" class=\"message\" colspan=\"2\">The User Name and Password combination you have entered is invalid. Please try again.</td></tr>    <tr> <td class=\"caption\" colspan=\"2\">&nbsp;</td></tr>");
	} else {
		document.writeln(" ");
	}
}
function unhideform() {
	document.getElementById("formId").style.display = "block";
}
