"""Read a private AT-SPI bus with gdbus; retain native evidence, not tree mocks."""
import re
import os
import subprocess

def run(*args):
    return subprocess.check_output(['gdbus','call',*args],text=True,stderr=subprocess.STDOUT,timeout=3).strip()

def enable():
    run('--session','--dest','org.a11y.Bus','--object-path','/org/a11y/bus','--method','org.freedesktop.DBus.Properties.Set','org.a11y.Status','IsEnabled','<true>')
    run('--session','--dest','org.a11y.Bus','--object-path','/org/a11y/bus','--method','org.freedesktop.DBus.Properties.Set','org.a11y.Status','ScreenReaderEnabled','<true>')
    if os.environ.get('AT_SPI_BUS_ADDRESS'):return os.environ['AT_SPI_BUS_ADDRESS']
    result=run('--session','--dest','org.a11y.Bus','--object-path','/org/a11y/bus','--method','org.a11y.Bus.GetAddress')
    return re.search("'([^']+)'",result)[1]

def tree(address):
    nodes=[]
    def call(bus,path,method,*args):
        return run('--address',address,'--dest',bus,'--object-path',path,'--method',method,*args)
    def visit(bus,path,depth):
        if depth>12 or len(nodes)>=300:return
        name=call(bus,path,'org.freedesktop.DBus.Properties.Get','org.a11y.atspi.Accessible','Name')
        role=call(bus,path,'org.a11y.atspi.Accessible.GetRole')
        nodes.append(dict(bus=bus,path=path,name=name,role=role))
        children=call(bus,path,'org.a11y.atspi.Accessible.GetChildren')
        for child in re.findall(r"\('([^']+)',\s*(?:objectpath )?'([^']+)'\)",children):
            visit(*child,depth+1)
    visit('org.a11y.atspi.Registry','/org/a11y/atspi/accessible/root',0)
    return nodes
