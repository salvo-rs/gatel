<?php
header('Content-Type: text/plain; charset=utf-8');
$body = isset($_GET['echo']) ? file_get_contents('php://input') : "Hello from FastCGI\n";
header('Content-Length: ' . strlen($body));
if ($_SERVER['REQUEST_METHOD'] !== 'HEAD') {
    echo $body;
}
