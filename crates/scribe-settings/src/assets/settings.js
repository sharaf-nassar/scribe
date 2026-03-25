// Scribe Settings UI — JavaScript

// ─────────── IPC ───────────

function sendChange(key, value) {
  if (window.ipc && window.ipc.postMessage) {
    window.ipc.postMessage(JSON.stringify({ type: "setting_changed", key, value }));
  }
}

// ─────────── State ───────────

let currentConfig = {};
let keybindingDefaults = {};
let recordingEl = null;
let recordingPrev = null;
let recordingPrevText = null;

// ─────────── Community Themes ───────────

/* Community theme data generated from Tabby terminal color schemes. */
/* Each entry: id (underscore), name (display), bg, fg, c1-c4 (green, blue, red, yellow) */
var COMMUNITY_THEMES = [
  {id:"3024_day",name:"3024 Day",bg:"#f7f7f7",fg:"#4a4543",c1:"#01a252",c2:"#01a0e4",c3:"#db2d20",c4:"#fded02"},
  {id:"3024_night",name:"3024 Night",bg:"#090300",fg:"#a5a2a2",c1:"#01a252",c2:"#01a0e4",c3:"#db2d20",c4:"#fded02"},
  {id:"adventuretime",name:"AdventureTime",bg:"#1f1d45",fg:"#f8dcc0",c1:"#4ab118",c2:"#0f4ac6",c3:"#bd0013",c4:"#e7741e"},
  {id:"afterglow",name:"Afterglow",bg:"#212121",fg:"#d0d0d0",c1:"#7e8e50",c2:"#6c99bb",c3:"#ac4142",c4:"#e5b567"},
  {id:"alienblood",name:"AlienBlood",bg:"#0f1610",fg:"#637d75",c1:"#2f7e25",c2:"#2f6a7f",c3:"#7f2b27",c4:"#717f24"},
  {id:"argonaut",name:"Argonaut",bg:"#0e1019",fg:"#fffaf4",c1:"#8ce10b",c2:"#008df8",c3:"#ff000f",c4:"#ffb900"},
  {id:"arthur",name:"Arthur",bg:"#1c1c1c",fg:"#ddeedd",c1:"#86af80",c2:"#6495ed",c3:"#cd5c5c",c4:"#e8ae5b"},
  {id:"ateliersulphurpool",name:"AtelierSulphurpool",bg:"#202746",fg:"#979db4",c1:"#ac9739",c2:"#3d8fd1",c3:"#c94922",c4:"#c08b30"},
  {id:"atom",name:"Atom",bg:"#161719",fg:"#c5c8c6",c1:"#87c38a",c2:"#85befd",c3:"#fd5ff1",c4:"#ffd7b1"},
  {id:"atomonelight",name:"AtomOneLight",bg:"#f9f9f9",fg:"#2a2c33",c1:"#3f953a",c2:"#2f5af3",c3:"#de3e35",c4:"#d2b67c"},
  {id:"base16_default_dark",name:"Base16 Default Dark",bg:"#181818",fg:"#d8d8d8",c1:"#a1b56c",c2:"#7cafc2",c3:"#ab4642",c4:"#f7ca88"},
  {id:"batman",name:"Batman",bg:"#1b1d1e",fg:"#6f6f6f",c1:"#c8be46",c2:"#737174",c3:"#e6dc44",c4:"#f4fd22"},
  {id:"belafonte_day",name:"Belafonte Day",bg:"#d5ccba",fg:"#45373c",c1:"#858162",c2:"#426a79",c3:"#be100e",c4:"#eaa549"},
  {id:"belafonte_night",name:"Belafonte Night",bg:"#20111b",fg:"#968c83",c1:"#858162",c2:"#426a79",c3:"#be100e",c4:"#eaa549"},
  {id:"birdsofparadise",name:"BirdsOfParadise",bg:"#2a1f1d",fg:"#e0dbb7",c1:"#6ba18a",c2:"#5a86ad",c3:"#be2d26",c4:"#e99d2a"},
  {id:"blazer",name:"Blazer",bg:"#0d1926",fg:"#d9e6f2",c1:"#7ab87a",c2:"#7a7ab8",c3:"#b87a7a",c4:"#b8b87a"},
  {id:"borland",name:"Borland",bg:"#0000a4",fg:"#ffff4e",c1:"#a8ff60",c2:"#96cbfe",c3:"#ff6c60",c4:"#ffffb6"},
  {id:"bright_lights",name:"Bright Lights",bg:"#191919",fg:"#b3c9d7",c1:"#b7e876",c2:"#76d4ff",c3:"#ff355b",c4:"#ffc251"},
  {id:"broadcast",name:"Broadcast",bg:"#2b2b2b",fg:"#e6e1dc",c1:"#519f50",c2:"#6d9cbe",c3:"#da4939",c4:"#ffd24a"},
  {id:"brogrammer",name:"Brogrammer",bg:"#131313",fg:"#d6dbe5",c1:"#2dc55e",c2:"#2a84d2",c3:"#f81118",c4:"#ecba0f"},
  {id:"c64",name:"C64",bg:"#40318d",fg:"#7869c4",c1:"#55a049",c2:"#40318d",c3:"#883932",c4:"#bfce72"},
  {id:"clrs",name:"CLRS",bg:"#ffffff",fg:"#262626",c1:"#328a5d",c2:"#135cd0",c3:"#f8282a",c4:"#fa701d"},
  {id:"chalk",name:"Chalk",bg:"#2b2d2e",fg:"#d2d8d9",c1:"#789b6a",c2:"#2a7fac",c3:"#b23a52",c4:"#b9ac4a"},
  {id:"chalkboard",name:"Chalkboard",bg:"#29262f",fg:"#d9e6f2",c1:"#72c373",c2:"#7372c3",c3:"#c37372",c4:"#c2c372"},
  {id:"ciapre",name:"Ciapre",bg:"#191c27",fg:"#aea47a",c1:"#48513b",c2:"#576d8c",c3:"#810009",c4:"#cc8b3f"},
  {id:"cobalt_neon",name:"Cobalt Neon",bg:"#142838",fg:"#8ff586",c1:"#3ba5ff",c2:"#8ff586",c3:"#ff2320",c4:"#e9e75c"},
  {id:"cobalt2",name:"Cobalt2",bg:"#132738",fg:"#ffffff",c1:"#38de21",c2:"#1460d2",c3:"#ff0000",c4:"#ffe50a"},
  {id:"crayonponyfish",name:"CrayonPonyFish",bg:"#150707",fg:"#68525a",c1:"#579524",c2:"#8c87b0",c3:"#91002b",c4:"#ab311b"},
  {id:"dark_pastel",name:"Dark Pastel",bg:"#000000",fg:"#ffffff",c1:"#55ff55",c2:"#5555ff",c3:"#ff5555",c4:"#ffff55"},
  {id:"darkside",name:"Darkside",bg:"#222324",fg:"#bababa",c1:"#68c256",c2:"#1c98e8",c3:"#e8341c",c4:"#f2d42c"},
  {id:"desert",name:"Desert",bg:"#333333",fg:"#ffffff",c1:"#98fb98",c2:"#cd853f",c3:"#ff2b2b",c4:"#f0e68c"},
  {id:"dimmedmonokai",name:"DimmedMonokai",bg:"#1f1f1f",fg:"#b9bcba",c1:"#879a3b",c2:"#4f76a1",c3:"#be3f48",c4:"#c5a635"},
  {id:"dotgov",name:"DotGov",bg:"#262c35",fg:"#ebebeb",c1:"#3d9751",c2:"#17b2e0",c3:"#bf091d",c4:"#f6bb34"},
  {id:"duotone_dark",name:"Duotone Dark",bg:"#1f1d27",fg:"#b7a1ff",c1:"#2dcd73",c2:"#ffc284",c3:"#d9393e",c4:"#d9b76e"},
  {id:"encom",name:"ENCOM",bg:"#000000",fg:"#00a595",c1:"#008b00",c2:"#0081ff",c3:"#9f0000",c4:"#ffd000"},
  {id:"earthsong",name:"Earthsong",bg:"#292520",fg:"#e5c7a9",c1:"#85c54c",c2:"#1398b9",c3:"#c94234",c4:"#f5ae2e"},
  {id:"elemental",name:"Elemental",bg:"#22211d",fg:"#807a74",c1:"#479a43",c2:"#497f7d",c3:"#98290f",c4:"#7f7111"},
  {id:"elementary",name:"Elementary",bg:"#181818",fg:"#efefef",c1:"#5aa513",c2:"#063b8c",c3:"#d71c15",c4:"#fdb40c"},
  {id:"espresso",name:"Espresso",bg:"#323232",fg:"#ffffff",c1:"#a5c261",c2:"#6c99bb",c3:"#d25252",c4:"#ffc66d"},
  {id:"espresso_libre",name:"Espresso Libre",bg:"#2a211c",fg:"#b8a898",c1:"#1a921c",c2:"#0066ff",c3:"#cc0000",c4:"#f0e53a"},
  {id:"fideloper",name:"Fideloper",bg:"#292f33",fg:"#dbdae0",c1:"#edb8ac",c2:"#2e78c2",c3:"#cb1e2d",c4:"#b7ab9b"},
  {id:"firefoxdev",name:"FirefoxDev",bg:"#0e1011",fg:"#7c8fa4",c1:"#5eb83c",c2:"#359ddf",c3:"#e63853",c4:"#a57706"},
  {id:"firewatch",name:"Firewatch",bg:"#1e2027",fg:"#9ba2b2",c1:"#5ab977",c2:"#4d89c4",c3:"#d95360",c4:"#dfb563"},
  {id:"fishtank",name:"FishTank",bg:"#232537",fg:"#ecf0fe",c1:"#acf157",c2:"#525fb8",c3:"#c6004a",c4:"#fecd5e"},
  {id:"flat",name:"Flat",bg:"#002240",fg:"#2cc55d",c1:"#32a548",c2:"#3167ac",c3:"#a82320",c4:"#e58d11"},
  {id:"flatland",name:"Flatland",bg:"#1d1f21",fg:"#b8dbef",c1:"#9fd364",c2:"#5096be",c3:"#f18339",c4:"#f4ef6d"},
  {id:"floraverse",name:"Floraverse",bg:"#0e0d15",fg:"#dbd1b9",c1:"#5d731a",c2:"#1d6da1",c3:"#64002c",c4:"#cd751c"},
  {id:"forestblue",name:"ForestBlue",bg:"#051519",fg:"#e2d8cd",c1:"#92d3a2",c2:"#8ed0ce",c3:"#f8818e",c4:"#1a8e63"},
  {id:"frontenddelight",name:"FrontEndDelight",bg:"#1b1c1d",fg:"#adadad",c1:"#565747",c2:"#2c70b7",c3:"#f8511b",c4:"#fa771d"},
  {id:"funforrest",name:"FunForrest",bg:"#251200",fg:"#dec165",c1:"#919c00",c2:"#4699a3",c3:"#d6262b",c4:"#be8a13"},
  {id:"galaxy",name:"Galaxy",bg:"#1d2837",fg:"#ffffff",c1:"#21b089",c2:"#589df6",c3:"#f9555f",c4:"#fef02a"},
  {id:"github",name:"Github",bg:"#f4f4f4",fg:"#3e3e3e",c1:"#07962a",c2:"#003e8a",c3:"#970b16",c4:"#f8eec7"},
  {id:"glacier",name:"Glacier",bg:"#0c1115",fg:"#ffffff",c1:"#35a770",c2:"#1f5872",c3:"#bd0f2f",c4:"#fb9435"},
  {id:"grape",name:"Grape",bg:"#171423",fg:"#9f9fa1",c1:"#1fa91b",c2:"#487df4",c3:"#ed2261",c4:"#8ddc20"},
  {id:"grass",name:"Grass",bg:"#13773d",fg:"#fff0a5",c1:"#00bb00",c2:"#0000a3",c3:"#bb0000",c4:"#e7b000"},
  {id:"gruvbox_dark",name:"Gruvbox Dark",bg:"#1e1e1e",fg:"#e6d4a3",c1:"#aab01e",c2:"#719586",c3:"#f73028",c4:"#f7b125"},
  {id:"hardcore",name:"Hardcore",bg:"#121212",fg:"#a0a0a0",c1:"#a6e22e",c2:"#66d9ef",c3:"#f92672",c4:"#fd971f"},
  {id:"harper",name:"Harper",bg:"#010101",fg:"#a8a49d",c1:"#7fb5e1",c2:"#489e48",c3:"#f8b63f",c4:"#d6da25"},
  {id:"highway",name:"Highway",bg:"#222225",fg:"#ededed",c1:"#138034",c2:"#006bb3",c3:"#d00e18",c4:"#ffcb3e"},
  {id:"hipster_green",name:"Hipster Green",bg:"#100b05",fg:"#84c138",c1:"#00a600",c2:"#246eb2",c3:"#b6214a",c4:"#bfbf00"},
  {id:"homebrew",name:"Homebrew",bg:"#000000",fg:"#00ff00",c1:"#00a600",c2:"#0000b2",c3:"#990000",c4:"#999900"},
  {id:"hurtado",name:"Hurtado",bg:"#000000",fg:"#dbdbdb",c1:"#a5e055",c2:"#496487",c3:"#ff1b00",c4:"#fbe74a"},
  {id:"hybrid",name:"Hybrid",bg:"#161719",fg:"#b7bcba",c1:"#b3bf5a",c2:"#6e90b0",c3:"#b84d51",c4:"#e4b55e"},
  {id:"ic_green_ppl",name:"IC_Green_PPL",bg:"#3a3d3f",fg:"#d9efd3",c1:"#339c24",c2:"#149b45",c3:"#fb002a",c4:"#659b25"},
  {id:"ic_orange_ppl",name:"IC_Orange_PPL",bg:"#262626",fg:"#ffcb83",c1:"#a4a900",c2:"#bd6d00",c3:"#c13900",c4:"#caaf00"},
  {id:"ir_black",name:"IR_Black",bg:"#000000",fg:"#f1f1f1",c1:"#a8ff60",c2:"#96cafe",c3:"#fa6c60",c4:"#fffeb7"},
  {id:"iceberg",name:"Iceberg",bg:"#161821",fg:"#c6c8d1",c1:"#b4be82",c2:"#84a0c6",c3:"#e27878",c4:"#e2a478"},
  {id:"jackie_brown",name:"Jackie Brown",bg:"#2c1d16",fg:"#ffcc2f",c1:"#2baf2b",c2:"#246eb2",c3:"#ef5734",c4:"#bebf00"},
  {id:"japanesque",name:"Japanesque",bg:"#1e1e1e",fg:"#f7f6ec",c1:"#7bb75b",c2:"#4c9ad4",c3:"#cf3f61",c4:"#e9b32a"},
  {id:"jazz",name:"Jazz",bg:"#f3f2f1",fg:"#3f3f3f",c1:"#668e3d",c2:"#8c71bf",c3:"#e06c75",c4:"#c49041"},
  {id:"jellybeans",name:"Jellybeans",bg:"#121212",fg:"#dedede",c1:"#94b979",c2:"#97bedc",c3:"#e27373",c4:"#ffba7b"},
  {id:"jetbrains_darcula",name:"JetBrains Darcula",bg:"#202020",fg:"#adadad",c1:"#126e00",c2:"#4581eb",c3:"#fa5355",c4:"#c2c300"},
  {id:"kibble",name:"Kibble",bg:"#0e100a",fg:"#f7f7f7",c1:"#29cf13",c2:"#3449d1",c3:"#c70031",c4:"#d8e30e"},
  {id:"later_this_evening",name:"Later This Evening",bg:"#222222",fg:"#959595",c1:"#afba67",c2:"#a0bad6",c3:"#d45a60",c4:"#e5d289"},
  {id:"lavandula",name:"Lavandula",bg:"#050014",fg:"#736e7d",c1:"#337e6f",c2:"#4f4a7f",c3:"#7d1625",c4:"#7f6f49"},
  {id:"light_owl",name:"Light Owl",bg:"#fbfbfb",fg:"#403f53",c1:"#08916a",c2:"#288ed7",c3:"#de3d3b",c4:"#e0af02"},
  {id:"liquidcarbon",name:"LiquidCarbon",bg:"#303030",fg:"#afc2c2",c1:"#559a70",c2:"#0099cc",c3:"#ff3030",c4:"#ccac00"},
  {id:"liquidcarbontransparent",name:"LiquidCarbonTransparent",bg:"#000000",fg:"#afc2c2",c1:"#559a70",c2:"#0099cc",c3:"#ff3030",c4:"#ccac00"},
  {id:"liquidcarbontransparentinverse",name:"LiquidCarbonTransparentInverse",bg:"#000000",fg:"#afc2c2",c1:"#559a70",c2:"#0099cc",c3:"#ff3030",c4:"#ccac00"},
  {id:"man_page",name:"Man Page",bg:"#fef49c",fg:"#000000",c1:"#00a600",c2:"#0000b2",c3:"#cc0000",c4:"#999900"},
  {id:"material",name:"Material",bg:"#eaeaea",fg:"#232322",c1:"#457b24",c2:"#134eb2",c3:"#b7141f",c4:"#f6981e"},
  {id:"materialdark",name:"MaterialDark",bg:"#232322",fg:"#e5e5e5",c1:"#457b24",c2:"#134eb2",c3:"#b7141f",c4:"#f6981e"},
  {id:"mathias",name:"Mathias",bg:"#000000",fg:"#bbbbbb",c1:"#a6e32d",c2:"#c48dff",c3:"#e52222",c4:"#fc951e"},
  {id:"medallion",name:"Medallion",bg:"#1d1908",fg:"#cac296",c1:"#7c8b16",c2:"#616bb0",c3:"#b64c00",c4:"#d3bd26"},
  {id:"melange_dark",name:"Melange Dark",bg:"#2a2520",fg:"#ece1d7",c1:"#78997a",c2:"#9aacce",c3:"#b65c60",c4:"#ebc06d"},
  {id:"misterioso",name:"Misterioso",bg:"#2d3743",fg:"#e1e1e0",c1:"#74af68",c2:"#338f86",c3:"#ff4242",c4:"#ffad29"},
  {id:"molokai",name:"Molokai",bg:"#121212",fg:"#bbbbbb",c1:"#98e123",c2:"#1080d0",c3:"#fa2573",c4:"#dfd460"},
  {id:"monalisa",name:"MonaLisa",bg:"#120b0d",fg:"#f7d66a",c1:"#636232",c2:"#515c5d",c3:"#9b291c",c4:"#c36e28"},
  {id:"monokai_soda",name:"Monokai Soda",bg:"#1a1a1a",fg:"#c4c5b5",c1:"#98e024",c2:"#9d65ff",c3:"#f4005f",c4:"#fa8419"},
  {id:"monokai_vivid",name:"Monokai Vivid",bg:"#121212",fg:"#f9f9f9",c1:"#98e123",c2:"#0443ff",c3:"#fa2934",c4:"#fff30a"},
  {id:"n0tch2k",name:"N0tch2k",bg:"#222222",fg:"#a0a0a0",c1:"#666666",c2:"#657d3e",c3:"#a95551",c4:"#a98051"},
  {id:"neopolitan",name:"Neopolitan",bg:"#271f19",fg:"#ffffff",c1:"#61ce3c",c2:"#253b76",c3:"#800000",c4:"#fbde2d"},
  {id:"neutron",name:"Neutron",bg:"#1c1e22",fg:"#e6e8ef",c1:"#5ab977",c2:"#6a7c93",c3:"#b54036",c4:"#deb566"},
  {id:"night_owl",name:"Night Owl",bg:"#011627",fg:"#d6deeb",c1:"#22da6e",c2:"#82aaff",c3:"#ef5350",c4:"#addb67"},
  {id:"nightlion_v1",name:"NightLion v1",bg:"#000000",fg:"#bbbbbb",c1:"#5fde8f",c2:"#276bd8",c3:"#bb0000",c4:"#f3f167"},
  {id:"nightlion_v2",name:"NightLion v2",bg:"#171717",fg:"#bbbbbb",c1:"#04f623",c2:"#64d0f0",c3:"#bb0000",c4:"#f3f167"},
  {id:"nord",name:"Nord",bg:"#2e3440",fg:"#d8dee9",c1:"#a3be8c",c2:"#81a1c1",c3:"#bf616a",c4:"#ebcb8b"},
  {id:"novel",name:"Novel",bg:"#dfdbc3",fg:"#3b2322",c1:"#009600",c2:"#0000cc",c3:"#cc0000",c4:"#d06b00"},
  {id:"obsidian",name:"Obsidian",bg:"#283033",fg:"#cdcdcd",c1:"#00bb00",c2:"#3a9bdb",c3:"#a60001",c4:"#fecd22"},
  {id:"ocean",name:"Ocean",bg:"#224fbc",fg:"#ffffff",c1:"#00a600",c2:"#0000b2",c3:"#990000",c4:"#999900"},
  {id:"oceanicmaterial",name:"OceanicMaterial",bg:"#1c262b",fg:"#c2c8d7",c1:"#40a33f",c2:"#1e80f0",c3:"#ee2b2a",c4:"#ffea2e"},
  {id:"ollie",name:"Ollie",bg:"#222125",fg:"#8a8dae",c1:"#31ac61",c2:"#2d57ac",c3:"#ac2e31",c4:"#ac4300"},
  {id:"onehalfdark",name:"OneHalfDark",bg:"#282c34",fg:"#dcdfe4",c1:"#98c379",c2:"#61afef",c3:"#e06c75",c4:"#e5c07b"},
  {id:"onehalflight",name:"OneHalfLight",bg:"#fafafa",fg:"#383a42",c1:"#50a14f",c2:"#0184bc",c3:"#e45649",c4:"#c18401"},
  {id:"pandora",name:"Pandora",bg:"#141e43",fg:"#e1e1e1",c1:"#74af68",c2:"#338f86",c3:"#ff4242",c4:"#ffad29"},
  {id:"paraiso_dark",name:"Paraiso Dark",bg:"#2f1e2e",fg:"#a39e9b",c1:"#48b685",c2:"#06b6ef",c3:"#ef6155",c4:"#fec418"},
  {id:"parasio_dark",name:"Parasio Dark",bg:"#2f1e2e",fg:"#a39e9b",c1:"#48b685",c2:"#06b6ef",c3:"#ef6155",c4:"#fec418"},
  {id:"paulmillr",name:"PaulMillr",bg:"#000000",fg:"#f2f2f2",c1:"#79ff0f",c2:"#396bd7",c3:"#ff0000",c4:"#e7bf00"},
  {id:"pencildark",name:"PencilDark",bg:"#212121",fg:"#f1f1f1",c1:"#10a778",c2:"#008ec4",c3:"#c30771",c4:"#a89c14"},
  {id:"pencillight",name:"PencilLight",bg:"#f1f1f1",fg:"#424242",c1:"#10a778",c2:"#008ec4",c3:"#c30771",c4:"#a89c14"},
  {id:"piatto_light",name:"Piatto Light",bg:"#ffffff",fg:"#414141",c1:"#66781e",c2:"#3c5ea8",c3:"#b23771",c4:"#cd6f34"},
  {id:"pnevma",name:"Pnevma",bg:"#1c1c1c",fg:"#d0d0d0",c1:"#90a57d",c2:"#7fa5bd",c3:"#a36666",c4:"#d7af87"},
  {id:"pro",name:"Pro",bg:"#000000",fg:"#f2f2f2",c1:"#00a600",c2:"#2009db",c3:"#990000",c4:"#999900"},
  {id:"red_alert",name:"Red Alert",bg:"#762423",fg:"#ffffff",c1:"#71be6b",c2:"#489bee",c3:"#d62e4e",c4:"#beb86b"},
  {id:"red_sands",name:"Red Sands",bg:"#7a251e",fg:"#d7c9a7",c1:"#00bb00",c2:"#0072ff",c3:"#ff3f00",c4:"#e7b000"},
  {id:"relaxed",name:"Relaxed",bg:"#343a43",fg:"#d8d8d8",c1:"#909d62",c2:"#698698",c3:"#bb5653",c4:"#eac179"},
  {id:"rippedcasts",name:"Rippedcasts",bg:"#2b2b2b",fg:"#ffffff",c1:"#a8ff60",c2:"#75a5b0",c3:"#cdaf95",c4:"#bfbb1f"},
  {id:"rose_pine",name:"Rose Pine",bg:"#191724",fg:"#e0def4",c1:"#31748f",c2:"#9ccfd8",c3:"#eb6f92",c4:"#f6c177"},
  {id:"rose_pine_dawn",name:"Rose Pine Dawn",bg:"#faf4ed",fg:"#575279",c1:"#286983",c2:"#56949f",c3:"#b4637a",c4:"#ea9d34"},
  {id:"rose_pine_moon",name:"Rose Pine Moon",bg:"#232136",fg:"#e0def4",c1:"#3e8fb0",c2:"#9ccfd8",c3:"#eb6f92",c4:"#f6c177"},
  {id:"royal",name:"Royal",bg:"#100815",fg:"#514968",c1:"#23801c",c2:"#6580b0",c3:"#91284c",c4:"#b49d27"},
  {id:"ryuuko",name:"Ryuuko",bg:"#2c3941",fg:"#ececec",c1:"#66907d",c2:"#6a8e95",c3:"#865f5b",c4:"#b1a990"},
  {id:"seashells",name:"SeaShells",bg:"#09141b",fg:"#deb88d",c1:"#027c9b",c2:"#1e4950",c3:"#d15123",c4:"#fca02f"},
  {id:"seafoam_pastel",name:"Seafoam Pastel",bg:"#243435",fg:"#d4e7d4",c1:"#728c62",c2:"#4d7b82",c3:"#825d4d",c4:"#ada16d"},
  {id:"seti",name:"Seti",bg:"#111213",fg:"#cacecd",c1:"#8ec43d",c2:"#43a5d5",c3:"#c22832",c4:"#e0c64f"},
  {id:"shaman",name:"Shaman",bg:"#001015",fg:"#405555",c1:"#00a941",c2:"#449a86",c3:"#b2302d",c4:"#5e8baa"},
  {id:"slate",name:"Slate",bg:"#222222",fg:"#35b1d2",c1:"#81d778",c2:"#264b49",c3:"#e2a8bf",c4:"#c4c9c0"},
  {id:"smyck",name:"Smyck",bg:"#1b1b1b",fg:"#f7f7f7",c1:"#7da900",c2:"#62a3c4",c3:"#b84131",c4:"#c4a500"},
  {id:"softserver",name:"SoftServer",bg:"#242626",fg:"#99a3a2",c1:"#9aa56a",c2:"#6b8fa3",c3:"#a2686a",c4:"#a3906a"},
  {id:"solarized_darcula",name:"Solarized Darcula",bg:"#3d3f41",fg:"#d2d8d9",c1:"#629655",c2:"#2075c7",c3:"#f24840",c4:"#b68800"},
  {id:"solarized_dark_patched",name:"Solarized Dark - Patched",bg:"#001e27",fg:"#708284",c1:"#738a05",c2:"#2176c7",c3:"#d11c24",c4:"#a57706"},
  {id:"solarized_dark_higher_contrast",name:"Solarized Dark Higher Contrast",bg:"#001e27",fg:"#9cc2c3",c1:"#6cbe6c",c2:"#2176c7",c3:"#d11c24",c4:"#a57706"},
  {id:"solarized_light",name:"Solarized Light",bg:"#fcf4dc",fg:"#536870",c1:"#738a05",c2:"#2176c7",c3:"#d11c24",c4:"#a57706"},
  {id:"spacegray",name:"SpaceGray",bg:"#20242d",fg:"#b3b8c3",c1:"#87b379",c2:"#7d8fa4",c3:"#b04b57",c4:"#e5c179"},
  {id:"spacegray_eighties",name:"SpaceGray Eighties",bg:"#222222",fg:"#bdbaae",c1:"#81a764",c2:"#5486c0",c3:"#ec5f67",c4:"#fec254"},
  {id:"spacegray_eighties_dull",name:"SpaceGray Eighties Dull",bg:"#222222",fg:"#c9c6bc",c1:"#92b477",c2:"#7c8fa5",c3:"#b24a56",c4:"#c6735a"},
  {id:"spacedust",name:"Spacedust",bg:"#0a1e24",fg:"#ecf0c1",c1:"#5cab96",c2:"#0f548b",c3:"#e35b00",c4:"#e3cd7b"},
  {id:"spiderman",name:"Spiderman",bg:"#1b1d1e",fg:"#e3e3e3",c1:"#e22928",c2:"#2c3fff",c3:"#e60813",c4:"#e24756"},
  {id:"spring",name:"Spring",bg:"#ffffff",fg:"#4d4d4c",c1:"#1f8c3b",c2:"#1dd3ee",c3:"#ff4d83",c4:"#1fc95b"},
  {id:"square",name:"Square",bg:"#1a1a1a",fg:"#acacab",c1:"#b6377d",c2:"#a9cdeb",c3:"#e9897c",c4:"#ecebbe"},
  {id:"sundried",name:"Sundried",bg:"#1a1818",fg:"#c9c9c9",c1:"#587744",c2:"#485b98",c3:"#a7463d",c4:"#9d602a"},
  {id:"symfonic",name:"Symfonic",bg:"#000000",fg:"#ffffff",c1:"#56db3a",c2:"#0084d4",c3:"#dc322f",c4:"#ff8400"},
  {id:"tango",name:"Tango",bg:"#000000",fg:"#babdb6",c1:"#4e9a06",c2:"#3465a4",c3:"#cc0000",c4:"#c4a000"},
  {id:"teerb",name:"Teerb",bg:"#262626",fg:"#d0d0d0",c1:"#aed686",c2:"#86aed6",c3:"#d68686",c4:"#d7af87"},
  {id:"terminal_basic",name:"Terminal Basic",bg:"#ffffff",fg:"#000000",c1:"#00a600",c2:"#0000b2",c3:"#990000",c4:"#999900"},
  {id:"thayer_bright",name:"Thayer Bright",bg:"#1b1d1e",fg:"#f8f8f8",c1:"#4df840",c2:"#2757d6",c3:"#f92672",c4:"#f4fd22"},
  {id:"the_hulk",name:"The Hulk",bg:"#1b1d1e",fg:"#b5b5b5",c1:"#13ce30",c2:"#2525f5",c3:"#269d1b",c4:"#63e457"},
  {id:"tokyonight_day",name:"TokyoNight Day",bg:"#e1e2e7",fg:"#3760bf",c1:"#587539",c2:"#2e7de9",c3:"#f52a65",c4:"#8c6c3e"},
  {id:"tokyonight_storm",name:"TokyoNight Storm",bg:"#24283b",fg:"#c0caf5",c1:"#9ece6a",c2:"#7aa2f7",c3:"#f7768e",c4:"#e0af68"},
  {id:"tomorrow",name:"Tomorrow",bg:"#ffffff",fg:"#4d4d4c",c1:"#718c00",c2:"#4271ae",c3:"#c82829",c4:"#eab700"},
  {id:"tomorrow_night",name:"Tomorrow Night",bg:"#1d1f21",fg:"#c5c8c6",c1:"#b5bd68",c2:"#81a2be",c3:"#cc6666",c4:"#f0c674"},
  {id:"tomorrow_night_blue",name:"Tomorrow Night Blue",bg:"#002451",fg:"#ffffff",c1:"#d1f1a9",c2:"#bbdaff",c3:"#ff9da4",c4:"#ffeead"},
  {id:"tomorrow_night_bright",name:"Tomorrow Night Bright",bg:"#000000",fg:"#eaeaea",c1:"#b9ca4a",c2:"#7aa6da",c3:"#d54e53",c4:"#e7c547"},
  {id:"tomorrow_night_eighties",name:"Tomorrow Night Eighties",bg:"#2d2d2d",fg:"#cccccc",c1:"#99cc99",c2:"#6699cc",c3:"#f2777a",c4:"#ffcc66"},
  {id:"toychest",name:"ToyChest",bg:"#24364b",fg:"#31d07b",c1:"#1a9172",c2:"#325d96",c3:"#be2d26",c4:"#db8e27"},
  {id:"treehouse",name:"Treehouse",bg:"#191919",fg:"#786b53",c1:"#44a900",c2:"#58859a",c3:"#b2270e",c4:"#aa820c"},
  {id:"twilight",name:"Twilight",bg:"#141414",fg:"#ffffd4",c1:"#afb97a",c2:"#44474a",c3:"#c06d44",c4:"#c2a86c"},
  {id:"ubuntu",name:"Ubuntu",bg:"#300a24",fg:"#eeeeec",c1:"#4e9a06",c2:"#3465a4",c3:"#cc0000",c4:"#c4a000"},
  {id:"underthesea",name:"UnderTheSea",bg:"#011116",fg:"#ffffff",c1:"#00a941",c2:"#459a86",c3:"#b2302d",c4:"#59819c"},
  {id:"urple",name:"Urple",bg:"#1b1b23",fg:"#877a9b",c1:"#37a415",c2:"#564d9b",c3:"#b0425b",c4:"#ad5c42"},
  {id:"vaughn",name:"Vaughn",bg:"#25234f",fg:"#dcdccc",c1:"#60b48a",c2:"#5555ff",c3:"#705050",c4:"#dfaf8f"},
  {id:"vibrantink",name:"VibrantInk",bg:"#000000",fg:"#ffffff",c1:"#ccff04",c2:"#44b4cc",c3:"#ff6600",c4:"#ffcc00"},
  {id:"violet_dark",name:"Violet Dark",bg:"#1c1d1f",fg:"#708284",c1:"#85981c",c2:"#2e8bce",c3:"#c94c22",c4:"#b4881d"},
  {id:"violet_light",name:"Violet Light",bg:"#fcf4dc",fg:"#536870",c1:"#85981c",c2:"#2e8bce",c3:"#c94c22",c4:"#b4881d"},
  {id:"warmneon",name:"WarmNeon",bg:"#404040",fg:"#afdab6",c1:"#39b13a",c2:"#4261c5",c3:"#e24346",c4:"#dae145"},
  {id:"wez",name:"Wez",bg:"#000000",fg:"#b3b3b3",c1:"#55cc55",c2:"#5555cc",c3:"#cc5555",c4:"#cdcd55"},
  {id:"wildcherry",name:"WildCherry",bg:"#1f1726",fg:"#dafaff",c1:"#2ab250",c2:"#883cdc",c3:"#d94085",c4:"#ffd16f"},
  {id:"wombat",name:"Wombat",bg:"#171717",fg:"#dedacf",c1:"#b1e969",c2:"#5da9f6",c3:"#ff615a",c4:"#ebd99c"},
  {id:"wryan",name:"Wryan",bg:"#101010",fg:"#999993",c1:"#287373",c2:"#395573",c3:"#8c4665",c4:"#7c7c99"},
  {id:"zenburn",name:"Zenburn",bg:"#3f3f3f",fg:"#dcdccc",c1:"#60b48a",c2:"#506070",c3:"#705050",c4:"#f0dfaf"},
  {id:"ayu",name:"ayu",bg:"#0f1419",fg:"#e6e1cf",c1:"#b8cc52",c2:"#36a3d9",c3:"#ff3333",c4:"#e7c547"},
  {id:"ayu_light",name:"ayu_light",bg:"#fafafa",fg:"#5c6773",c1:"#86b300",c2:"#41a6d9",c3:"#ff3333",c4:"#f29718"},
  {id:"base2tone_cave_dark",name:"base2tone-cave-dark",bg:"#222021",fg:"#9f999b",c1:"#cca133",c2:"#9c818b",c3:"#936c7a",c4:"#ffcc4d"},
  {id:"base2tone_desert_dark",name:"base2tone-desert-dark",bg:"#292724",fg:"#ada594",c1:"#ec9255",c2:"#957e50",c3:"#816f4b",c4:"#ffb380"},
  {id:"base2tone_drawbridge_dark",name:"base2tone-drawbridge-dark",bg:"#1b1f32",fg:"#9094a7",c1:"#67c9e4",c2:"#7289fd",c3:"#627af4",c4:"#99e9ff"},
  {id:"base2tone_evening_dark",name:"base2tone-evening-dark",bg:"#2a2734",fg:"#a4a1b5",c1:"#ffad5c",c2:"#9a86fd",c3:"#8a75f5",c4:"#ffcc99"},
  {id:"base2tone_forest_dark",name:"base2tone-forest-dark",bg:"#2a2d2a",fg:"#a1b5a1",c1:"#bfd454",c2:"#687d68",c3:"#5c705c",c4:"#e5fb79"},
  {id:"base2tone_heath_dark",name:"base2tone-heath-dark",bg:"#222022",fg:"#9e999f",c1:"#cc8c33",c2:"#9a819c",c3:"#8f6c93",c4:"#ffd599"},
  {id:"base2tone_heath_light",name:"base2tone-heath-light",bg:"#fbfaf9",fg:"#575158",c1:"#cc8c33",c2:"#9a819c",c3:"#8f6c93",c4:"#ffd599"},
  {id:"base2tone_lake_dark",name:"base2tone-lake-dark",bg:"#192d34",fg:"#7ba8b7",c1:"#cbbb4d",c2:"#499fbc",c3:"#3e91ac",c4:"#ffeb66"},
  {id:"base2tone_meadow_dark",name:"base2tone-meadow-dark",bg:"#192834",fg:"#7b9eb7",c1:"#80bf40",c2:"#4299d7",c3:"#277fbe",c4:"#a6f655"},
  {id:"base2tone_morning_light",name:"base2tone-morning-light",bg:"#faf8f5",fg:"#4f5664",c1:"#b29762",c2:"#3d75e6",c3:"#1659df",c4:"#e5ddcd"},
  {id:"base2tone_pool_dark",name:"base2tone-pool-dark",bg:"#2a2433",fg:"#9a90a7",c1:"#f87972",c2:"#b886fd",c3:"#aa75f5",c4:"#ffb6b3"},
  {id:"base2tone_sea_dark",name:"base2tone-sea-dark",bg:"#1d262f",fg:"#a1aab5",c1:"#0fc78a",c2:"#57718e",c3:"#34659d",c4:"#47ebb4"},
  {id:"base2tone_space_dark",name:"base2tone-space-dark",bg:"#24242e",fg:"#a1a1b5",c1:"#ec7336",c2:"#767693",c3:"#7676f4",c4:"#fe8c52"},
  {id:"deep",name:"deep",bg:"#000000",fg:"#cdcdcd",c1:"#37c32c",c2:"#5c6bfd",c3:"#d11600",c4:"#e3c421"},
  {id:"idletoes",name:"idleToes",bg:"#323232",fg:"#ffffff",c1:"#7fe173",c2:"#4099ff",c3:"#d25252",c4:"#ffc66d"},
];

function makeColorSpan(text, color) {
  var span = document.createElement("span");
  span.textContent = text;
  span.style.color = color;
  return span;
}

function makePreviewLine(parts) {
  var div = document.createElement("div");
  parts.forEach(function(p, i) {
    if (i > 0) { div.appendChild(document.createTextNode(" ")); }
    div.appendChild(makeColorSpan(p.text, p.color));
  });
  return div;
}

function initCommunityThemes() {
  var grid = document.getElementById("community-grid");
  var search = document.getElementById("community-search");
  if (!grid || !search) { return; }

  function renderCards(filter) {
    while (grid.firstChild) { grid.removeChild(grid.firstChild); }
    var lower = (filter || "").toLowerCase();
    COMMUNITY_THEMES.forEach(function(t) {
      if (lower && t.name.toLowerCase().indexOf(lower) === -1) { return; }
      var card = document.createElement("div");
      card.className = "theme-card";
      card.setAttribute("data-theme", t.id);

      var preview = document.createElement("div");
      preview.className = "theme-preview";
      preview.style.background = t.bg;

      preview.appendChild(makePreviewLine([
        {text: "$", color: t.c1}, {text: "cargo build", color: t.fg}
      ]));
      preview.appendChild(makePreviewLine([
        {text: "src/", color: t.c2}, {text: "Cargo.toml", color: t.c4}
      ]));
      preview.appendChild(makePreviewLine([
        {text: "error", color: t.c3}, {text: "fn", color: t.c2}
      ]));

      var nameEl = document.createElement("div");
      nameEl.className = "theme-name";
      var nameSpan = document.createElement("span");
      nameSpan.textContent = t.name;
      var checkDiv = document.createElement("div");
      checkDiv.className = "theme-check";
      checkDiv.style.display = "none";
      checkDiv.textContent = "\u2713";
      nameEl.appendChild(nameSpan);
      nameEl.appendChild(checkDiv);

      card.appendChild(preview);
      card.appendChild(nameEl);
      grid.appendChild(card);

      card.addEventListener("click", function() {
        document.querySelectorAll(".theme-card").forEach(function(c) {
          c.classList.remove("selected");
          var ck = c.querySelector(".theme-check");
          if (ck) { ck.style.display = "none"; }
        });
        card.classList.add("selected");
        checkDiv.style.display = "flex";
        sendChange("theme.preset", t.id);
      });
    });
  }

  renderCards("");

  search.addEventListener("input", function() {
    renderCards(search.value);
  });
}

// ─────────── Font List (injected by Rust) ───────────

function loadFontList(fonts) {
  const select = document.querySelector('select[data-key="appearance.font_family"]');
  if (!select) return;

  const currentValue = select.value;

  // Clear existing options safely (no innerHTML).
  while (select.firstChild) {
    select.removeChild(select.firstChild);
  }

  for (const name of fonts) {
    const opt = document.createElement("option");
    opt.value = name;
    opt.textContent = name;
    select.appendChild(opt);
  }

  // Always include a System Monospace fallback.
  const fallback = document.createElement("option");
  fallback.value = "monospace";
  fallback.textContent = "System Monospace";
  select.appendChild(fallback);

  // Restore the previously selected value if it exists in the new list.
  const configValue = currentConfig.appearance?.font || currentValue;
  if (configValue) {
    const match = Array.from(select.options).find(o => o.value === configValue);
    if (match) {
      select.value = configValue;
    } else {
      // Font not installed — add it as a visible entry so the user knows.
      const missing = document.createElement("option");
      missing.value = configValue;
      missing.textContent = configValue + " (not found)";
      select.insertBefore(missing, select.firstChild);
      select.value = configValue;
    }
  }
}

function requestFontRefresh() {
  if (window.ipc && window.ipc.postMessage) {
    window.ipc.postMessage(JSON.stringify({ type: "request_fonts" }));
  }
}

// ─────────── Keybinding Defaults (injected by Rust) ───────────

function loadKeybindingDefaults(defaults) {
  keybindingDefaults = defaults;
}

// ─────────── Tab Navigation ───────────

function initNavigation() {
  const navItems = document.querySelectorAll(".nav-item");
  const pages = document.querySelectorAll(".content-page");

  navItems.forEach(function(item) {
    item.addEventListener("click", function() {
      const target = item.getAttribute("data-tab");

      navItems.forEach(function(n) { n.classList.remove("active"); });
      item.classList.add("active");

      pages.forEach(function(p) {
        if (p.id === "page-" + target) {
          p.classList.add("active");
        } else {
          p.classList.remove("active");
        }
      });
    });
  });
}

// ─────────── Number Steppers ───────────

function initSteppers() {
  document.querySelectorAll(".number-control").forEach(function(ctrl) {
    const key = ctrl.getAttribute("data-key");
    const step = parseFloat(ctrl.getAttribute("data-step") || "1");
    const min = parseFloat(ctrl.getAttribute("data-min") || "0");
    const max = parseFloat(ctrl.getAttribute("data-max") || "99999");

    // Replace the static <div> with an <input> so users can type values.
    const oldDiv = ctrl.querySelector(".number-value");
    const input = document.createElement("input");
    input.type = "text";
    input.inputMode = "numeric";
    input.className = "number-value";
    input.value = oldDiv.textContent.trim();
    oldDiv.replaceWith(input);

    const valueEl = input;
    const btns = ctrl.querySelectorAll(".number-btn");

    function clamp(v) {
      var n = parseFloat(v);
      if (isNaN(n)) { n = min; }
      n = Math.max(min, Math.min(max, n));
      n = Math.round(n / step) * step;
      return n;
    }

    btns[0].addEventListener("click", function() {
      var val = clamp(parseFloat(valueEl.value) - step);
      valueEl.value = String(val);
      sendChange(key, val);
    });

    btns[1].addEventListener("click", function() {
      var val = clamp(parseFloat(valueEl.value) + step);
      valueEl.value = String(val);
      sendChange(key, val);
    });

    valueEl.addEventListener("blur", function() {
      var val = clamp(valueEl.value);
      valueEl.value = String(val);
      sendChange(key, val);
    });

    valueEl.addEventListener("keydown", function(e) {
      if (e.key === "Enter") {
        e.preventDefault();
        valueEl.blur();
      }
    });
  });
}

// ─────────── Toggles ───────────

function initToggles() {
  document.querySelectorAll(".toggle").forEach(function(toggle) {
    const key = toggle.getAttribute("data-key");

    toggle.addEventListener("click", function() {
      const isOn = toggle.classList.contains("on");

      if (isOn) {
        toggle.classList.remove("on");
        toggle.classList.add("off");
      } else {
        toggle.classList.remove("off");
        toggle.classList.add("on");
      }

      sendChange(key, !isOn);
    });
  });
}

// ─────────── Segmented Controls ───────────

function initSegmented() {
  document.querySelectorAll(".segmented-control").forEach(function(ctrl) {
    const key = ctrl.getAttribute("data-key");
    const opts = ctrl.querySelectorAll(".segment-opt");

    opts.forEach(function(opt) {
      opt.addEventListener("click", function() {
        opts.forEach(function(o) { o.classList.remove("active"); });
        opt.classList.add("active");
        sendChange(key, opt.getAttribute("data-value"));
      });
    });
  });
}

// ─────────── Sliders ───────────

function initSliders() {
  document.querySelectorAll("input[type='range']").forEach(function(slider) {
    const key = slider.getAttribute("data-key");
    const display = slider.parentElement.querySelector(".slider-val");
    const precision = parseInt(slider.getAttribute("data-precision") || "1", 10);

    slider.addEventListener("input", function() {
      const val = parseFloat(slider.value);
      display.textContent = val.toFixed(precision);
    });

    slider.addEventListener("change", function() {
      sendChange(key, parseFloat(slider.value));
    });
  });
}

// ─────────── Select Dropdowns ───────────

function initSelects() {
  document.querySelectorAll("select.select-control").forEach(function(sel) {
    const key = sel.getAttribute("data-key");

    sel.addEventListener("change", function() {
      sendChange(key, sel.value);
    });
  });
}

// ─────────── Text Inputs ───────────

function initTextInputs() {
  document.querySelectorAll("input.text-input").forEach(function(input) {
    const key = input.getAttribute("data-key");

    input.addEventListener("change", function() {
      sendChange(key, input.value);
    });
  });
}

// ─────────── Theme Cards ───────────

function initThemeCards() {
  const cards = document.querySelectorAll(".theme-card");

  cards.forEach(function(card) {
    card.addEventListener("click", function() {
      cards.forEach(function(c) { c.classList.remove("selected"); });
      card.classList.add("selected");

      // Show/hide checkmark
      cards.forEach(function(c) {
        const check = c.querySelector(".theme-check");
        if (check) { check.style.display = "none"; }
      });
      const check = card.querySelector(".theme-check");
      if (check) { check.style.display = "flex"; }

      const themeName = card.getAttribute("data-theme");
      sendChange("theme.preset", themeName);
    });
  });
}

// ─────────── Color Swatches ───────────

function initColorSwatches() {
  document.querySelectorAll(".color-swatch").forEach(function(swatch) {
    const key = swatch.getAttribute("data-key");
    const colorInput = swatch.querySelector("input[type='color']");
    if (!colorInput || !key) { return; }

    colorInput.addEventListener("input", function() {
      swatch.style.backgroundColor = colorInput.value;
    });

    colorInput.addEventListener("change", function() {
      sendChange(key, colorInput.value);
    });
  });

  document.querySelectorAll(".ansi-swatch").forEach(function(swatch) {
    const key = swatch.getAttribute("data-key");
    const colorInput = swatch.querySelector("input[type='color']");
    if (!colorInput || !key) { return; }

    colorInput.addEventListener("input", function() {
      swatch.style.backgroundColor = colorInput.value;
    });

    colorInput.addEventListener("change", function() {
      sendChange(key, colorInput.value);
    });
  });
}

// ─────────── Workspace Roots ───────────

function initWorkspaces() {
  const list = document.getElementById("workspace-list");
  if (!list) { return; }

  list.addEventListener("click", function(e) {
    const removeBtn = e.target.closest(".workspace-remove");
    if (removeBtn) {
      const item = removeBtn.closest(".workspace-item");
      const path = item.querySelector(".workspace-path").textContent;
      item.remove();
      sendChange("workspaces.remove_root", path);
      return;
    }

    const addBtn = e.target.closest(".workspace-add");
    if (addBtn) {
      sendChange("workspaces.add_root", "");
    }
  });
}

// ─────────── loadConfig — called by Rust ───────────

function loadConfig(config) {
  currentConfig = config;

  // Appearance — Typography
  setSelectValue("appearance.font_family", config.appearance?.font);
  setStepperValue("appearance.font_size", config.appearance?.font_size);
  setStepperValue("appearance.font_weight", config.appearance?.font_weight);
  setStepperValue("appearance.bold_weight", config.appearance?.bold_weight);
  setToggleValue("appearance.ligatures", config.appearance?.ligatures);
  setStepperValue("appearance.line_padding", config.appearance?.line_padding);

  // Appearance — Cursor
  setSegmentedValue("appearance.cursor_shape", config.appearance?.cursor_shape);
  setToggleValue("appearance.cursor_blink", config.appearance?.cursor_blink);

  // Appearance — Window
  setSliderValue("appearance.opacity", config.appearance?.opacity);
  setStepperValue("appearance.tab_bar_padding", config.appearance?.tab_bar_padding);

  // Terminal
  setStepperValue("terminal.scrollback_lines", config.terminal?.scrollback_lines);
  setToggleValue("terminal.natural_scroll", config.terminal?.natural_scroll);
  setToggleValue("terminal.copy_on_select", config.terminal?.copy_on_select);
  setToggleValue("terminal.claude_copy_cleanup", config.terminal?.claude_copy_cleanup);
  setToggleValue("terminal.claude_code_integration", config.terminal?.claude_code_integration);
  setStepperValue("terminal.indicator_height", config.terminal?.indicator_height);

  // Claude Code States
  var states = config.terminal?.claude_states;
  if (states) {
    ["processing","idle_prompt","waiting_for_input","permission_prompt","error"]
      .forEach(function(s) {
        var e = states[s];
        if (!e) { return; }
        setToggleValue("claude_states." + s + ".tab_indicator", e.tab_indicator);
        setToggleValue("claude_states." + s + ".pane_border", e.pane_border);
        // Color swatches only accept hex; ansi:N values are left at default.
        if (typeof e.color === "string" && e.color.charAt(0) === "#") {
          setColorSwatch("claude_states." + s + ".color", e.color);
        }
        setStepperValue("claude_states." + s + ".pulse_ms", e.pulse_ms);
        setStepperValue("claude_states." + s + ".timeout_secs", e.timeout_secs);
      });
  }

  // Theme — appearance.theme is kebab-case, data-theme attrs use underscores
  var presetId = config.appearance?.theme;
  if (presetId) { presetId = presetId.replace(/-/g, "_"); }
  setThemePreset(presetId);
  setColorSwatch("theme.foreground", config.theme?.foreground);
  setColorSwatch("theme.background", config.theme?.background);
  setColorSwatch("theme.cursor", config.theme?.cursor);
  setColorSwatch("theme.cursor_text", config.theme?.cursor_text);
  setColorSwatch("theme.selection", config.theme?.selection);
  setColorSwatch("theme.selection_text", config.theme?.selection_text);

  // ANSI colors
  if (config.theme?.ansi_normal) {
    config.theme.ansi_normal.forEach(function(color, i) {
      setAnsiSwatch("theme.ansi_normal." + i, color);
    });
  }
  if (config.theme?.ansi_bright) {
    config.theme.ansi_bright.forEach(function(color, i) {
      setAnsiSwatch("theme.ansi_bright." + i, color);
    });
  }

  // Keybindings — values are now arrays of combo strings
  if (config.keybindings) {
    Object.keys(config.keybindings).forEach(function(action) {
      var val = config.keybindings[action];
      var list = Array.isArray(val) ? val : (val ? [val] : []);
      setKeybindingValue(action, list);
    });
  }

  // Workspaces
  if (config.workspaces?.roots) {
    populateWorkspaceRoots(config.workspaces.roots);
  }
}

// ─────────── Value Setters ───────────

function setSelectValue(key, value) {
  if (value === undefined || value === null) { return; }
  var el = document.querySelector("select[data-key='" + key + "']");
  if (el) { el.value = value; }
}

function setStepperValue(key, value) {
  if (value === undefined || value === null) { return; }
  var ctrl = document.querySelector(".number-control[data-key='" + key + "']");
  if (ctrl) {
    var valEl = ctrl.querySelector(".number-value");
    if (valEl) { valEl.value = String(value); }
  }
}

function setToggleValue(key, value) {
  if (value === undefined || value === null) { return; }
  var toggle = document.querySelector(".toggle[data-key='" + key + "']");
  if (toggle) {
    toggle.classList.remove("on", "off");
    toggle.classList.add(value ? "on" : "off");
  }
}

function setSegmentedValue(key, value) {
  if (value === undefined || value === null) { return; }
  var ctrl = document.querySelector(".segmented-control[data-key='" + key + "']");
  if (ctrl) {
    ctrl.querySelectorAll(".segment-opt").forEach(function(opt) {
      if (opt.getAttribute("data-value") === value) {
        opt.classList.add("active");
      } else {
        opt.classList.remove("active");
      }
    });
  }
}

function setSliderValue(key, value) {
  if (value === undefined || value === null) { return; }
  var slider = document.querySelector("input[type='range'][data-key='" + key + "']");
  if (slider) {
    slider.value = value;
    var display = slider.parentElement.querySelector(".slider-val");
    var precision = parseInt(slider.getAttribute("data-precision") || "1", 10);
    if (display) { display.textContent = parseFloat(value).toFixed(precision); }
  }
}

function setTextValue(key, value) {
  if (value === undefined || value === null) { return; }
  var input = document.querySelector("input.text-input[data-key='" + key + "']");
  if (input) { input.value = value; }
}

function setThemePreset(preset) {
  if (!preset) { return; }
  var cards = document.querySelectorAll(".theme-card");
  cards.forEach(function(card) {
    var check = card.querySelector(".theme-check");
    if (card.getAttribute("data-theme") === preset) {
      card.classList.add("selected");
      if (check) { check.style.display = "flex"; }
    } else {
      card.classList.remove("selected");
      if (check) { check.style.display = "none"; }
    }
  });
}

function setColorSwatch(key, color) {
  if (!color) { return; }
  var swatch = document.querySelector(".color-swatch[data-key='" + key + "']");
  if (swatch) {
    swatch.style.backgroundColor = color;
    var input = swatch.querySelector("input[type='color']");
    if (input) { input.value = color; }
  }
}

function setAnsiSwatch(key, color) {
  if (!color) { return; }
  var swatch = document.querySelector(".ansi-swatch[data-key='" + key + "']");
  if (swatch) {
    swatch.style.backgroundColor = color;
    var input = swatch.querySelector("input[type='color']");
    if (input) { input.value = color; }
  }
}

function formatKeybinding(shortcut) {
  return shortcut.split("+").map(function(part) {
    var p = part.trim();
    if (p === "ctrl" || p === "shift" || p === "alt") {
      return p.charAt(0).toUpperCase() + p.slice(1);
    }
    return p.length === 1 ? p.toUpperCase() : p;
  }).join("+");
}

var MAX_BINDINGS = 5;

function setKeybindingValue(action, combos) {
  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (!cell) { return; }
  renderBadges(cell, action, combos);
}

function renderBadges(cell, action, combos) {
  // Remove existing badges, remove buttons, and add button — keep reset button.
  var toRemove = cell.querySelectorAll(".keybinding-key, .kb-remove-btn, .kb-add-btn");
  for (var i = 0; i < toRemove.length; i++) { toRemove[i].remove(); }

  var resetBtn = cell.querySelector(".kb-reset-btn");

  for (var idx = 0; idx < combos.length; idx++) {
    var badge = document.createElement("span");
    badge.className = "keybinding-key";
    badge.setAttribute("data-action", action);
    badge.setAttribute("data-index", String(idx));
    badge.setAttribute("data-current", combos[idx]);
    badge.textContent = formatKeybinding(combos[idx]);
    cell.insertBefore(badge, resetBtn);

    var removeBtn = document.createElement("button");
    removeBtn.className = "kb-remove-btn";
    removeBtn.setAttribute("data-action", action);
    removeBtn.setAttribute("data-index", String(idx));
    removeBtn.title = "Remove binding";
    removeBtn.textContent = "\u00d7";
    cell.insertBefore(removeBtn, resetBtn);
  }

  if (combos.length < MAX_BINDINGS) {
    var addBtn = document.createElement("button");
    addBtn.className = "kb-add-btn";
    addBtn.setAttribute("data-action", action);
    addBtn.title = "Add binding";
    addBtn.textContent = "+";
    cell.insertBefore(addBtn, resetBtn);
  }

  // Re-apply search highlighting if a search query is active.
  var searchInput = document.getElementById("kb-search");
  var query = searchInput ? searchInput.value.trim().toLowerCase() : "";
  if (query) {
    var badges = cell.querySelectorAll(".keybinding-key");
    for (var h = 0; h < badges.length; h++) {
      highlightText(badges[h], query);
    }
  }
}

function getCombosForAction(action) {
  var badges = document.querySelectorAll(".keybinding-key[data-action='" + action + "']");
  var combos = [];
  for (var i = 0; i < badges.length; i++) {
    var c = badges[i].getAttribute("data-current");
    if (c) { combos.push(c); }
  }
  return combos;
}

function populateWorkspaceRoots(roots) {
  var list = document.getElementById("workspace-list");
  if (!list) { return; }

  // Remove existing items but keep the add button
  var addBtn = list.querySelector(".workspace-add");
  var items = list.querySelectorAll(".workspace-item");
  items.forEach(function(item) { item.remove(); });

  // Build items using safe DOM methods
  roots.forEach(function(path) {
    var item = document.createElement("div");
    item.className = "workspace-item";

    var pathSpan = document.createElement("span");
    pathSpan.className = "workspace-path";
    pathSpan.textContent = path;
    item.appendChild(pathSpan);

    var removeButton = document.createElement("button");
    removeButton.className = "workspace-remove";
    removeButton.title = "Remove";

    var svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("width", "14");
    svg.setAttribute("height", "14");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");

    var line1 = document.createElementNS("http://www.w3.org/2000/svg", "line");
    line1.setAttribute("x1", "18");
    line1.setAttribute("y1", "6");
    line1.setAttribute("x2", "6");
    line1.setAttribute("y2", "18");

    var line2 = document.createElementNS("http://www.w3.org/2000/svg", "line");
    line2.setAttribute("x1", "6");
    line2.setAttribute("y1", "6");
    line2.setAttribute("x2", "18");
    line2.setAttribute("y2", "18");

    svg.appendChild(line1);
    svg.appendChild(line2);
    removeButton.appendChild(svg);
    item.appendChild(removeButton);

    list.insertBefore(item, addBtn);
  });
}

// ─────────── Keybinding Recording ───────────

var KEY_NAMES = {
  " ": "space",
  "ArrowLeft": "left",
  "ArrowRight": "right",
  "ArrowUp": "up",
  "ArrowDown": "down",
  "PageUp": "pageup",
  "PageDown": "pagedown",
  "Home": "home",
  "End": "end",
  "Backspace": "backspace",
  "Delete": "delete",
  "Enter": "enter",
  "Tab": "tab",
  "Escape": "escape"
};

var MODIFIER_KEYS = ["Control", "Shift", "Alt", "Meta"];

function buildComboString(e) {
  // Ignore modifier-only keypresses.
  if (MODIFIER_KEYS.indexOf(e.key) !== -1) {
    return null;
  }

  var parts = [];
  if (e.ctrlKey || e.metaKey) { parts.push("ctrl"); }
  if (e.shiftKey) { parts.push("shift"); }
  if (e.altKey) { parts.push("alt"); }

  var keyName = KEY_NAMES[e.key];
  if (!keyName) {
    if (e.key.length === 1) {
      keyName = e.key.toLowerCase();
    } else {
      return null;
    }
  }

  parts.push(keyName);
  return parts.join("+");
}

function startRecording(el) {
  // Cancel any active recording first.
  if (recordingEl) {
    cancelRecording();
  }

  recordingPrev = el.getAttribute("data-current") || "";
  recordingPrevText = el.textContent;
  el.classList.add("recording");
  el.textContent = "Press keys...";
  recordingEl = el;
}

function cancelRecording() {
  if (!recordingEl) { return; }
  var action = recordingEl.getAttribute("data-action");
  var isNew = !recordingPrev;
  recordingEl.classList.remove("recording");

  if (isNew) {
    // Was a newly added placeholder — remove it and re-render.
    var combos = getCombosForAction(action).filter(function(c) { return c; });
    var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
    recordingEl = null;
    recordingPrev = null;
    recordingPrevText = null;
    if (cell) { renderBadges(cell, action, combos); }
  } else {
    recordingEl.textContent = formatKeybinding(recordingPrev);
    recordingEl = null;
    recordingPrev = null;
    recordingPrevText = null;
  }
}

function finishRecording(combo) {
  if (!recordingEl) { return; }
  var action = recordingEl.getAttribute("data-action");
  var idx = parseInt(recordingEl.getAttribute("data-index"), 10);

  // Collect current combos and update the recorded index.
  var combos = getCombosForAction(action);
  if (idx < combos.length) {
    combos[idx] = combo;
  } else {
    combos.push(combo);
  }

  recordingEl.classList.remove("recording");
  recordingEl = null;
  recordingPrev = null;
  recordingPrevText = null;

  // Re-render and persist.
  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (cell) { renderBadges(cell, action, combos); }
  sendChange("keybindings." + action, combos);
}

function removeKeybinding(action, idx) {
  var combos = getCombosForAction(action);
  combos.splice(idx, 1);
  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (cell) { renderBadges(cell, action, combos); }
  sendChange("keybindings." + action, combos);
  hideConflictWarning();
}

function addKeybinding(action) {
  var combos = getCombosForAction(action);
  if (combos.length >= MAX_BINDINGS) { return; }

  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (!cell) { return; }

  // Create a placeholder badge at the next index and start recording on it.
  var newIdx = combos.length;
  var badge = document.createElement("span");
  badge.className = "keybinding-key";
  badge.setAttribute("data-action", action);
  badge.setAttribute("data-index", String(newIdx));
  badge.setAttribute("data-current", "");

  // Insert before the add button (or reset button).
  var addBtn = cell.querySelector(".kb-add-btn");
  var resetBtn = cell.querySelector(".kb-reset-btn");
  cell.insertBefore(badge, addBtn || resetBtn);

  // Remove the add button while recording.
  if (addBtn) { addBtn.remove(); }

  startRecording(badge);
}

function findConflict(sourceAction, combo) {
  var normalized = combo.toLowerCase();
  var all = document.querySelectorAll(".keybinding-key[data-action]");
  for (var i = 0; i < all.length; i++) {
    var el = all[i];
    // Skip all badges of the same action (multi-bind for same action is fine).
    if (el.getAttribute("data-action") === sourceAction) { continue; }
    var current = (el.getAttribute("data-current") || "").toLowerCase();
    if (current === normalized) {
      return el.getAttribute("data-action");
    }
  }
  return null;
}

function showConflictWarning(conflictAction, combo) {
  var banner = document.getElementById("kb-conflict-banner");
  if (!banner) { return; }
  var label = conflictAction.replace(/_/g, " ");
  banner.textContent = formatKeybinding(combo) + " is already assigned to " + label;
  banner.style.display = "block";
}

function hideConflictWarning() {
  var banner = document.getElementById("kb-conflict-banner");
  if (banner) { banner.style.display = "none"; }
}

function resetKeybinding(action) {
  var def = keybindingDefaults[action];
  if (!def) { return; }
  // Defaults are now arrays; get the first default combo.
  var firstDefault = Array.isArray(def) ? def[0] : def;
  if (!firstDefault) { return; }

  var combos = getCombosForAction(action);
  if (combos.length === 0) {
    combos = [firstDefault];
  } else {
    combos[0] = firstDefault;
  }

  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (cell) { renderBadges(cell, action, combos); }
  sendChange("keybindings." + action, combos);
  hideConflictWarning();
}

function initKeybindingRecorder() {
  // Click delegation for keybinding badges, add, remove, and reset.
  var page = document.getElementById("page-keybindings");
  if (!page) { return; }

  page.addEventListener("click", function(e) {
    var badge = e.target.closest(".keybinding-key");
    if (badge && !badge.classList.contains("kb-add-btn")) {
      e.stopPropagation();
      startRecording(badge);
      return;
    }

    var addBtn = e.target.closest(".kb-add-btn");
    if (addBtn) {
      e.stopPropagation();
      var addAction = addBtn.getAttribute("data-action");
      if (addAction) { addKeybinding(addAction); }
      return;
    }

    var removeBtn = e.target.closest(".kb-remove-btn");
    if (removeBtn) {
      e.stopPropagation();
      var rmAction = removeBtn.getAttribute("data-action");
      var rmIdx = parseInt(removeBtn.getAttribute("data-index"), 10);
      if (rmAction) { removeKeybinding(rmAction, rmIdx); }
      return;
    }

    var resetBtn = e.target.closest(".kb-reset-btn");
    if (resetBtn) {
      e.stopPropagation();
      var resetAction = resetBtn.getAttribute("data-action");
      if (resetAction) { resetKeybinding(resetAction); }
    }
  });

  // Global keydown listener for recording (capture phase).
  document.addEventListener("keydown", function(e) {
    if (!recordingEl) { return; }
    e.preventDefault();
    e.stopPropagation();

    if (e.key === "Escape") {
      cancelRecording();
      hideConflictWarning();
      return;
    }

    var combo = buildComboString(e);
    if (!combo) { return; }

    var action = recordingEl.getAttribute("data-action");
    var conflict = findConflict(action, combo);

    if (conflict) {
      showConflictWarning(conflict, combo);
    } else {
      hideConflictWarning();
    }

    finishRecording(combo);
  }, true);
}

// ─────────── Keybinding Search ───────────

function initKeybindingSearch() {
  var input = document.getElementById("kb-search");
  if (!input) { return; }

  input.addEventListener("input", function() {
    var query = input.value.trim().toLowerCase();
    filterKeybindingRows(query);
  });
}

function clearHighlights(container) {
  var marks = container.querySelectorAll(".kb-highlight");
  for (var i = 0; i < marks.length; i++) {
    var mark = marks[i];
    var parent = mark.parentNode;
    parent.replaceChild(document.createTextNode(mark.textContent), mark);
    parent.normalize();
  }
}

function highlightText(el, query) {
  if (!query || !el) { return; }
  var text = el.textContent;
  var lower = text.toLowerCase();
  var idx = lower.indexOf(query);
  if (idx === -1) { return; }

  var before = document.createTextNode(text.slice(0, idx));
  var mark = document.createElement("span");
  mark.className = "kb-highlight";
  mark.textContent = text.slice(idx, idx + query.length);
  var after = document.createTextNode(text.slice(idx + query.length));

  el.textContent = "";
  el.appendChild(before);
  el.appendChild(mark);
  el.appendChild(after);
}

function filterKeybindingRows(query) {
  var page = document.getElementById("page-keybindings");
  if (page) { clearHighlights(page); }

  var groups = document.querySelectorAll("#page-keybindings .section-group");
  groups.forEach(function(group) {
    var sectionLabel = group.querySelector(".section-label");
    var sectionText = sectionLabel ? sectionLabel.textContent.toLowerCase() : "";
    var sectionMatches = query && sectionText.indexOf(query) !== -1;
    var rows = group.querySelectorAll(".setting-row");
    var anyVisible = false;

    if (sectionMatches) {
      // Section header matches — show all rows in this section.
      rows.forEach(function(row) { row.style.display = ""; });
      anyVisible = true;
      if (query && sectionLabel) { highlightText(sectionLabel, query); }
    } else {
      rows.forEach(function(row) {
        var labelEl = row.querySelector(".setting-label");
        var keyEls = row.querySelectorAll(".keybinding-key");
        var label = labelEl ? labelEl.textContent : "";
        var keyText = "";
        for (var k = 0; k < keyEls.length; k++) {
          keyText += " " + keyEls[k].textContent;
        }
        var matches = !query ||
          label.toLowerCase().indexOf(query) !== -1 ||
          keyText.toLowerCase().indexOf(query) !== -1;

        row.style.display = matches ? "" : "none";
        if (matches && query) {
          highlightText(labelEl, query);
          for (var k2 = 0; k2 < keyEls.length; k2++) {
            highlightText(keyEls[k2], query);
          }
          anyVisible = true;
        } else if (matches) {
          anyVisible = true;
        }
      });
    }

    group.style.display = anyVisible ? "" : "none";
  });
}

// ─────────── Init ───────────

document.addEventListener("DOMContentLoaded", function() {
  initNavigation();
  initSteppers();
  initToggles();
  initSegmented();
  initSliders();
  initSelects();
  initTextInputs();
  initThemeCards();
  initCommunityThemes();
  initColorSwatches();
  initWorkspaces();
  initKeybindingSearch();
  initKeybindingRecorder();

  // Font refresh button.
  const refreshBtn = document.getElementById("refresh-fonts");
  if (refreshBtn) {
    refreshBtn.addEventListener("click", requestFontRefresh);
  }
});
